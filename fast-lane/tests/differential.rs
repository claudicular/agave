#![cfg(feature = "agave-unstable-api")]
//! Runtime differential test: the fast lane's per-transaction frames, statuses and final
//! state must equal agave's for the same child slot, whatever the schedule.
//!
//! Path A: FL executes slot N over frozen parent P (worker counts 1/2/6, speculation off,
//! realistic hint threshold, blind speculation).
//! Path B: agave executes the same transactions serially in block order on a real child
//! bank of P, with a capturing accounts-update notifier (grouped notifications).

use {
    agave_fast_lane::{
        control::Tunables,
        forks::FlForkGraph,
        mv::{accounts_equal, same_value},
        program_cache::ProgramCaches,
        run::{Run, TxOutcome},
        sched::{CoordMsg, Coordinator, FinalSink, Finalized, RunId, RunSummary, worker_loop},
    },
    rand::{Rng, SeedableRng, rngs::StdRng},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{
        accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING,
        accounts_update_notifier_interface::{AccountForGeyser, AccountsUpdateNotifierInterface},
    },
    solana_clock::{BankId, Slot},
    solana_entry::entry::Entry,
    solana_fee_structure::FeeStructure,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::Message,
    solana_nonce::{
        state::{DurableNonce, State as NonceState},
        versions::Versions as NonceVersions,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, SlotLeader},
        bank_forks::BankForks,
        genesis_utils::{GenesisConfigInfo, create_genesis_config},
        runtime_config::RuntimeConfig,
    },
    solana_signature::Signature,
    solana_signer::Signer,
    solana_svm::transaction_processor::ExecutionRecordingConfig,
    solana_svm_timings::ExecuteTimings,
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_transaction::{Transaction, TransactionVerificationMode, versioned::VersionedTransaction},
    solana_transaction_error::TransactionError,
    std::{
        collections::HashMap,
        sync::{
            Arc, Mutex, RwLock,
            atomic::AtomicBool,
        },
        thread,
        time::{Duration, Instant},
    },
};

const TOKEN_PROGRAM: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

#[derive(Debug, Default)]
struct Capture {
    frames: Mutex<HashMap<(Slot, Signature), Vec<(Pubkey, AccountSharedData)>>>,
}

impl AccountsUpdateNotifierInterface for Capture {
    fn snapshot_notifications_enabled(&self) -> bool {
        false
    }
    fn notify_account_update(
        &self,
        _slot: Slot,
        _bank_id: BankId,
        _account: &AccountSharedData,
        _txn: &Option<&solana_transaction::sanitized::SanitizedTransaction>,
        _pubkey: &Pubkey,
        _write_version: u64,
    ) {
    }
    fn notify_account_restore_from_snapshot(
        &self,
        _slot: Slot,
        _write_version: u64,
        _account: &AccountForGeyser<'_>,
    ) {
    }
    fn notify_end_of_restore_from_snapshot(&self) {}
    fn notify_transaction_accounts(
        &self,
        slot: Slot,
        _bank_id: BankId,
        signature: &Signature,
        _transaction_index: usize,
        accounts: &[(&Pubkey, &AccountSharedData)],
        _write_version_start: u64,
    ) {
        self.frames.lock().unwrap().insert(
            (slot, *signature),
            accounts
                .iter()
                .map(|(k, a)| (**k, (*a).clone()))
                .collect(),
        );
    }
    fn transaction_accounts_notifications_enabled(&self) -> bool {
        true
    }
    fn transaction_accounts_include_readonly_owners(&self) -> Vec<Pubkey> {
        vec![TOKEN_PROGRAM]
    }
}

struct World {
    parent: Arc<Bank>,
    bank_forks: Arc<RwLock<BankForks>>,
    capture: Arc<Capture>,
    keys: Vec<Keypair>,
    payers: Vec<Keypair>,
    nonce: Pubkey,
    nonce_authority: Keypair,
    nonce_hash: Hash,
}

fn world() -> World {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair: _,
        ..
    } = create_genesis_config(1_000_000_000_000_000);
    let capture = Arc::new(Capture::default());
    let mut bank0 = Bank::new_from_genesis(
        &genesis_config,
        Arc::new(RuntimeConfig::default()),
        Vec::new(),
        None,
        ACCOUNTS_DB_CONFIG_FOR_TESTING,
        Some(capture.clone()),
        None,
        Arc::default(),
        None,
        None,
    );
    bank0.set_fee_structure(&FeeStructure {
        lamports_per_signature: 5000,
        ..FeeStructure::default()
    });
    let (bank0, bank_forks) = bank0.wrap_with_bank_forks_for_tests();
    let parent =
        Bank::new_from_parent_with_bank_forks(&bank_forks, bank0, SlotLeader::default(), 1);
    let keys: Vec<Keypair> = (0..12).map(|_| Keypair::new()).collect();
    let payers: Vec<Keypair> = (0..4).map(|_| Keypair::new()).collect();
    for (i, key) in keys.iter().enumerate() {
        // Mixed balances so some transfers fail.
        let lamports = if i % 4 == 0 { 30_000 } else { 50_000_000_000 };
        parent.store_account(
            &key.pubkey(),
            &AccountSharedData::new(lamports, 0, &system_program::id()),
        );
    }
    for payer in &payers {
        parent.store_account(
            &payer.pubkey(),
            &AccountSharedData::new(10_000_000_000, 0, &system_program::id()),
        );
    }
    // A token-owned read-only account (exercises the readonly-owner frame rule).
    parent.store_account(
        &Pubkey::new_from_array([7; 32]),
        &AccountSharedData::new(1_000_000, 82, &TOKEN_PROGRAM),
    );
    // A durable nonce account whose nonce is not the next durable nonce.
    let nonce_authority = Keypair::new();
    let nonce = Pubkey::new_unique();
    let nonce_hash = Hash::new_unique();
    let durable = DurableNonce::from_blockhash(&nonce_hash);
    let state = NonceState::new_initialized(&nonce_authority.pubkey(), durable, 5000);
    let nonce_account =
        AccountSharedData::new_data(1_000_000_000, &NonceVersions::new(state), &system_program::id())
            .unwrap();
    parent.store_account(&nonce, &nonce_account);
    parent.store_account(
        &nonce_authority.pubkey(),
        &AccountSharedData::new(1_000_000_000, 0, &system_program::id()),
    );
    parent.freeze();
    World {
        parent,
        bank_forks,
        capture,
        keys,
        payers,
        nonce,
        nonce_authority,
        nonce_hash: *durable.as_hash(),
    }
}

/// Conflicting transfers, payer chains, failures, account creation and drain, one nonce use,
/// and read-only token-owned accounts.
fn transactions(w: &World, seed: u64) -> Vec<VersionedTransaction> {
    let mut rng = StdRng::seed_from_u64(seed);
    let blockhash = w.parent.last_blockhash();
    let token_ro = Pubkey::new_from_array([7; 32]);
    let mut txs = Vec::new();
    for i in 0..220u64 {
        let from = &w.keys[rng.random_range(0..w.keys.len())];
        let to = if rng.random_bool(0.1) {
            Pubkey::new_unique()
        } else {
            w.keys[rng.random_range(0..w.keys.len())].pubkey()
        };
        // `+ i` keeps every transaction unique (a block cannot repeat a signature).
        let lamports = match rng.random_range(0..10) {
            0 => 40_000_000_000 + i, // often exceeds the balance: failure
            1 => 1 + i,
            _ => rng.random_range(1_000..5_000_000) + i,
        };
        let use_payer = rng.random_bool(0.5);
        let mut ixs = vec![system_instruction::transfer(&from.pubkey(), &to, lamports)];
        if rng.random_bool(0.2) {
            // Mention the token-owned account read-only.
            let mut ix = system_instruction::transfer(&from.pubkey(), &to, 1);
            ix.accounts
                .push(solana_instruction::AccountMeta::new_readonly(token_ro, false));
            ixs.push(ix);
        }
        let tx = if use_payer {
            let payer = &w.payers[rng.random_range(0..w.payers.len())];
            let message = Message::new(&ixs, Some(&payer.pubkey()));
            Transaction::new(&[payer, from], message, blockhash)
        } else {
            let message = Message::new(&ixs, Some(&from.pubkey()));
            Transaction::new(&[from], message, blockhash)
        };
        txs.push(VersionedTransaction::from(tx));
        if i == 100 {
            // Drain an account completely; later transfers from it fail.
            let victim = &w.keys[1];
            let balance = 50_000_000_000u64;
            let tx = Transaction::new(
                &[&w.payers[0], victim],
                Message::new(
                    &[system_instruction::transfer(
                        &victim.pubkey(),
                        &w.keys[2].pubkey(),
                        balance,
                    )],
                    Some(&w.payers[0].pubkey()),
                ),
                blockhash,
            );
            txs.push(VersionedTransaction::from(tx));
        }
        if i == 150 {
            // One durable-nonce transaction.
            let tx = Transaction::new(
                &[&w.nonce_authority],
                Message::new(
                    &[
                        system_instruction::advance_nonce_account(
                            &w.nonce,
                            &w.nonce_authority.pubkey(),
                        ),
                        system_instruction::transfer(
                            &w.nonce_authority.pubkey(),
                            &w.keys[3].pubkey(),
                            12_345,
                        ),
                    ],
                    Some(&w.nonce_authority.pubkey()),
                ),
                w.nonce_hash,
            );
            txs.push(VersionedTransaction::from(tx));
        }
    }
    txs
}

#[derive(Default)]
struct Collect {
    outcomes: Vec<TxOutcome>,
    ended: Vec<RunSummary>,
}

struct CollectSink(Arc<Mutex<Collect>>);

impl FinalSink for CollectSink {
    fn on_final(&mut self, f: Finalized) {
        let outcome = *f.payload.downcast::<TxOutcome>().unwrap();
        self.0.lock().unwrap().outcomes.push(outcome);
    }
    fn on_run_end(&mut self, _run_id: RunId, summary: RunSummary) {
        self.0.lock().unwrap().ended.push(summary);
    }
}

fn run_fast_lane(
    w: &World,
    txs: &[VersionedTransaction],
    workers: usize,
    speculation: bool,
    theta: f32,
) -> (Vec<TxOutcome>, RunSummary, Arc<Run>) {
    let graph = Arc::new(RwLock::new(FlForkGraph::default()));
    graph.write().unwrap().set_parent(1, 0);
    let mut programs = ProgramCaches::new(graph);
    let run = Arc::new(
        Run::new(
            7,
            2,
            &w.parent,
            &mut programs,
            Arc::new(vec![TOKEN_PROGRAM]),
        )
        .unwrap(),
    );
    // Entries of up to 8 transactions, pushed in two halves (ingest arrives in data sets).
    let entries: Vec<Entry> = txs
        .chunks(8)
        .map(|chunk| Entry {
            num_hashes: 1,
            hash: Hash::default(),
            transactions: chunk.to_vec(),
        })
        .collect();
    let half = entries.len() / 2;
    let (metas_a, fail_a) = run.push_entries(entries[..half].to_vec(), Instant::now(), 0);
    let (metas_b, fail_b) = run.push_entries(entries[half..].to_vec(), Instant::now(), 0);
    assert!(fail_a.is_none() && fail_b.is_none());

    let (task_tx, task_rx) = crossbeam_channel::unbounded();
    let (coord_tx, coord_rx) = crossbeam_channel::unbounded();
    let exit = Arc::new(AtomicBool::new(false));
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let task_rx = task_rx.clone();
            let coord_tx = coord_tx.clone();
            let exit = exit.clone();
            thread::spawn(move || worker_loop(task_rx, coord_tx, exit, Duration::from_micros(10)))
        })
        .collect();
    let sink = Arc::new(Mutex::new(Collect::default()));
    let tunables = Arc::new(Tunables::new(speculation, true, theta, 3));
    let mut coord = Coordinator::new(
        workers,
        task_tx,
        tunables,
        1.0 / 32.0,
        CollectSink(sink.clone()),
    );
    let n_a = metas_a.len() as u32;
    let total = (metas_a.len() + metas_b.len()) as u32;
    for msg in [
        CoordMsg::NewRun {
            run_id: 7,
            run: run.clone(),
        },
        CoordMsg::Txs {
            run_id: 7,
            first: 0,
            metas: metas_a,
            t_ingest: Instant::now(),
        },
        CoordMsg::Txs {
            run_id: 7,
            first: n_a,
            metas: metas_b,
            t_ingest: Instant::now(),
        },
        CoordMsg::InputComplete { run_id: 7, total },
    ] {
        coord.handle(msg);
    }
    coord.dispatch();
    let deadline = Instant::now() + Duration::from_secs(120);
    while sink.lock().unwrap().ended.is_empty() {
        if let Ok(msg) = coord_rx.recv_timeout(Duration::from_millis(5)) {
            coord.handle(msg);
            while let Ok(msg) = coord_rx.try_recv() {
                coord.handle(msg);
            }
            coord.dispatch();
        }
        assert!(Instant::now() < deadline, "stuck: {:?}", coord.run_state_counts(7));
    }
    exit.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(coord);
    drop(coord_tx);
    for h in handles {
        h.join().unwrap();
    }
    let mut collect = sink.lock().unwrap();
    let summary = collect.ended[0].clone();
    let outcomes = std::mem::take(&mut collect.outcomes);
    (outcomes, summary, run)
}

/// Agave: execute the same transactions serially in a real child bank.
fn run_agave(
    w: &World,
    txs: &[VersionedTransaction],
) -> (Arc<Bank>, HashMap<Signature, Result<(), TransactionError>>) {
    let child = Bank::new_from_parent_with_bank_forks(
        &w.bank_forks,
        w.parent.clone(),
        SlotLeader::default(),
        2,
    );
    let mut statuses = HashMap::new();
    for tx in txs {
        let rtx = child
            .verify_transaction(tx.clone(), TransactionVerificationMode::HashOnly)
            .unwrap();
        let batch = child.prepare_sanitized_batch(std::slice::from_ref(&rtx));
        let (results, _) = child.load_execute_and_commit_transactions(
            &batch,
            ExecutionRecordingConfig::default(),
            &mut ExecuteTimings::default(),
            None,
        );
        let status = match &results[0] {
            Ok(committed) => committed.status.clone(),
            Err(err) => Err(err.clone()),
        };
        statuses.insert(tx.signatures[0], status);
    }
    (child, statuses)
}

fn assert_frames_equal(
    what: &str,
    fl: &Option<Vec<(Pubkey, AccountSharedData)>>,
    agave: Option<&Vec<(Pubkey, AccountSharedData)>>,
) {
    match (fl, agave) {
        (None, None) => {}
        (Some(fl), Some(agave)) => {
            assert_eq!(fl.len(), agave.len(), "{what}: frame length");
            for ((ka, a), (kb, b)) in fl.iter().zip(agave) {
                assert_eq!(ka, kb, "{what}: frame key order");
                assert!(accounts_equal(a, b), "{what}: account {ka}: {a:?} vs {b:?}");
            }
        }
        (fl, agave) => panic!(
            "{what}: frame presence differs (fl {}, agave {})",
            fl.is_some(),
            agave.is_some()
        ),
    }
}

#[test]
fn test_differential_against_agave() {
    let w = world();
    let txs = transactions(&w, 11);
    let (child, statuses) = run_agave(&w, &txs);
    let frames = w.capture.frames.lock().unwrap().clone();
    assert!(frames.len() > 150, "agave notified {} txs", frames.len());
    let failures = statuses.values().filter(|s| s.is_err()).count();
    assert!(failures > 5, "the workload must contain failures ({failures})");

    for (workers, speculation, theta) in [
        (1usize, false, 0.2f32),
        (2, true, 0.2),
        (6, true, 0.2),
        (6, true, 1000.0),
        (3, true, 0.5),
    ] {
        let what = format!("K={workers} spec={speculation} theta={theta}");
        let (outcomes, summary, run) = run_fast_lane(&w, &txs, workers, speculation, theta);
        assert_eq!(outcomes.len(), txs.len(), "{what}: every tx final");
        assert_eq!(summary.unprocessable, 0, "{what}");
        for outcome in &outcomes {
            let sig = outcome.signature;
            let agave_status = &statuses[&sig];
            assert_eq!(&outcome.status, agave_status, "{what}: status of {sig}");
            assert_frames_equal(
                &format!("{what} tx {} {sig}", outcome.ordinal),
                &outcome.frame,
                frames.get(&(2, sig)),
            );
        }
        // Final state of every account FL wrote equals agave's child bank.
        for key in run.overlay.written_keys() {
            let fl = run.overlay.latest(&key).value;
            let agave = child.get_account(&key);
            assert!(same_value(&fl, &agave), "{what}: final state of {key}");
        }
        if speculation && theta >= 1000.0 {
            assert!(summary.spec_dispatches > 0, "{what}: {summary:?}");
        }
        eprintln!("{what}: {summary:?}");
    }
    // Keep the capture's frames for agave-only statistics.
    let _ = child.slot();
    let _ = w.keys.len();
    let _ = ReadableAccount::lamports(&AccountSharedData::default());
}
