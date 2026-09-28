#![cfg(feature = "agave-unstable-api")]
//! Runtime differential test: the fast lane's per-transaction frames, statuses and final
//! state must equal agave's for the same child slot, whatever the schedule.
//!
//! Path A: FL executes slot N over frozen parent P (worker counts 1/2/6, speculation off,
//! realistic hint threshold, blind speculation).
//! Path B: agave executes the same transactions serially in block order on a real child
//! bank of P, with a capturing accounts-update notifier (grouped notifications).
//!
//! The chained test runs slot C on top of FL's own run of P while agave's bank P is not
//! frozen (phase 2b), resolving P's freeze-time writes and C's SlotHashes at different
//! points of C's execution; C contains transactions that depend on exactly those values
//! (votes on P, transfers to and from P's fee collector) and on P's new blockhash.

use {
    agave_fast_lane::{
        control::Tunables,
        forks::FlForkGraph,
        mv::{accounts_equal, same_value},
        out_ring::{self, OutRing, OutRingReader},
        output::{OutPublisher, OutStats},
        program_cache::ProgramCaches,
        run::{Run, TxOutcome},
        sched::{
            CoordMsg, Coordinator, ExecOutput, FinalSink, Finalized, RunId, RunSummary, TxMeta,
            worker_loop,
        },
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
        genesis_utils::{GenesisConfigInfo, create_genesis_config_with_leader},
        runtime_config::RuntimeConfig,
    },
    solana_signature::Signature,
    solana_signer::Signer,
    solana_svm::transaction_processor::ExecutionRecordingConfig,
    solana_svm_timings::ExecuteTimings,
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_transaction::{Transaction, TransactionVerificationMode, versioned::VersionedTransaction},
    solana_transaction_error::TransactionError,
    solana_vote_interface::{
        instruction as vote_instruction,
        state::{BLS_PUBLIC_KEY_COMPRESSED_SIZE, TowerSync},
    },
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
    /// Leader of the chained test's slot 2 (the genesis validator); funded.
    leader: Keypair,
    leader_vote: Pubkey,
    /// (authorized voter and fee payer, vote account).
    voters: Vec<(Keypair, Pubkey)>,
}

fn world() -> World {
    // The leader of the chained test's slot 2 is the genesis validator (its vote account is
    // in the epoch stakes, as SIMD-0232 fee collection requires).
    let leader = Keypair::new();
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair: _,
        voting_keypair,
        ..
    } = create_genesis_config_with_leader(1_000_000_000_000_000, &leader.pubkey(), 1_000_000_000_000);
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
    parent.store_account(
        &leader.pubkey(),
        &AccountSharedData::new(1_000_000_000, 0, &system_program::id()),
    );
    let voters: Vec<(Keypair, Pubkey)> = (0..2)
        .map(|_| {
            let voter = Keypair::new();
            let vote = Pubkey::new_unique();
            parent.store_account(
                &voter.pubkey(),
                &AccountSharedData::new(10_000_000_000, 0, &system_program::id()),
            );
            parent.store_account(
                &vote,
                &solana_vote_program::vote_state::create_v4_account_with_authorized(
                    &voter.pubkey(),
                    &voter.pubkey(),
                    [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                    &voter.pubkey(),
                    0,
                    &voter.pubkey(),
                    0,
                    &voter.pubkey(),
                    100_000_000_000,
                ),
            );
            (voter, vote)
        })
        .collect();
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
        leader,
        leader_vote: voting_keypair.pubkey(),
        voters,
    }
}

/// Conflicting transfers, payer chains, failures, account creation and drain, one nonce use,
/// and read-only token-owned accounts.
fn transactions(w: &World, seed: u64) -> Vec<VersionedTransaction> {
    transactions_with(w, seed, w.parent.last_blockhash(), w.nonce_hash)
}

fn transactions_with(
    w: &World,
    seed: u64,
    blockhash: Hash,
    nonce_hash: Hash,
) -> Vec<VersionedTransaction> {
    let mut rng = StdRng::seed_from_u64(seed);
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
                nonce_hash,
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

/// Collects outcomes and, when given a publisher, also writes the phase-3 output ring
/// exactly as the production sink does (on the coordinator thread, at FINAL).
struct CollectSink(Arc<Mutex<Collect>>, Option<OutPublisher>);

impl FinalSink for CollectSink {
    fn on_final(&mut self, f: Finalized) {
        // A chained run's pseudo-transaction has no outcome.
        if let Ok(outcome) = f.payload.downcast::<TxOutcome>() {
            if let Some(out) = self.1.as_mut() {
                out.on_final(f.run_id, &outcome, f.incarnations, f.speculative);
            }
            self.0.lock().unwrap().outcomes.push(*outcome);
        }
    }
    fn on_run_end(&mut self, run_id: RunId, summary: RunSummary) {
        if let Some(out) = self.1.as_mut() {
            out.on_run_end(run_id, &summary);
        }
        self.0.lock().unwrap().ended.push(summary);
    }
}

fn entries_of(txs: &[VersionedTransaction]) -> Vec<Entry> {
    // Entries of up to 8 transactions (ingest arrives in data sets).
    txs.chunks(8)
        .map(|chunk| Entry {
            num_hashes: 1,
            hash: Hash::default(),
            transactions: chunk.to_vec(),
        })
        .collect()
}

/// Push `txs` into `run` in two halves; returns the coordinator messages (first index,
/// metas) starting at the run's `ordinal_base`.
fn push_halves(run: &Run, txs: &[VersionedTransaction]) -> Vec<(u32, Vec<TxMeta>)> {
    let entries = entries_of(txs);
    let half = entries.len() / 2;
    let (metas_a, fail_a) = run.push_entries(entries[..half].to_vec(), Instant::now(), 0, false);
    let (metas_b, fail_b) = run.push_entries(entries[half..].to_vec(), Instant::now(), 0, false);
    assert!(fail_a.is_none() && fail_b.is_none());
    let first_b = run.ordinal_base + metas_a.len() as u32;
    vec![(run.ordinal_base, metas_a), (first_b, metas_b)]
}

/// A message produced once during the run: after `after_finals` outcomes, or when the run
/// makes no progress (every remaining transaction waits on it).
struct External {
    after_finals: usize,
    make: Box<dyn FnOnce() -> CoordMsg>,
}

fn drive(
    run: Arc<Run>,
    run_id: RunId,
    batches: Vec<(u32, Vec<TxMeta>)>,
    workers: usize,
    speculation: bool,
    theta: f32,
    rebase: bool,
    mut external: Option<External>,
    out: Option<OutPublisher>,
) -> (Vec<TxOutcome>, RunSummary) {
    let total = batches
        .iter()
        .map(|(first, metas)| first + metas.len() as u32)
        .max()
        .unwrap_or(0);
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
    let tunables = Arc::new(Tunables::new(speculation, true, theta, 3).with_rebase(rebase));
    let mut coord = Coordinator::new(
        workers,
        task_tx,
        tunables,
        1.0 / 32.0,
        CollectSink(sink.clone(), out),
    );
    coord.handle(CoordMsg::NewRun {
        run_id,
        run: run.clone(),
    });
    for (first, metas) in batches {
        coord.handle(CoordMsg::Txs {
            run_id,
            first,
            metas,
            t_ingest: Instant::now(),
        });
    }
    coord.handle(CoordMsg::InputComplete { run_id, total });
    coord.dispatch();
    let deadline = Instant::now() + Duration::from_secs(120);
    while sink.lock().unwrap().ended.is_empty() {
        let msg = coord_rx.recv_timeout(Duration::from_millis(5));
        let stalled = msg.is_err();
        if let Ok(msg) = msg {
            coord.handle(msg);
            while let Ok(msg) = coord_rx.try_recv() {
                coord.handle(msg);
            }
        }
        let finals = sink.lock().unwrap().outcomes.len();
        if external
            .as_ref()
            .is_some_and(|e| stalled || finals >= e.after_finals)
        {
            let e = external.take().unwrap();
            coord.handle((e.make)());
        }
        coord.dispatch();
        assert!(
            Instant::now() < deadline,
            "stuck: {:?}",
            coord.run_state_counts(run_id)
        );
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
    (outcomes, summary)
}

fn run_fast_lane(
    w: &World,
    txs: &[VersionedTransaction],
    workers: usize,
    speculation: bool,
    theta: f32,
    rebase: bool,
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
    let batches = push_halves(&run, txs);
    let (outcomes, summary) =
        drive(run.clone(), 7, batches, workers, speculation, theta, rebase, None, None);
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
    let statuses = execute_serially(&child, txs);
    (child, statuses)
}

fn execute_serially(
    child: &Bank,
    txs: &[VersionedTransaction],
) -> HashMap<Signature, Result<(), TransactionError>> {
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
    statuses
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

    for (workers, speculation, theta, rebase) in [
        (1usize, false, 0.2f32, false),
        (2, true, 0.2, false),
        (6, true, 0.2, false),
        (6, true, 1000.0, false),
        (3, true, 0.5, false),
        (2, true, 0.2, true),
        (3, true, 0.5, true),
        (8, true, 0.5, true),
        (6, true, 1000.0, true),
    ] {
        let what = format!("K={workers} spec={speculation} theta={theta} rebase={rebase}");
        let (outcomes, summary, run) =
            run_fast_lane(&w, &txs, workers, speculation, theta, rebase);
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
        if !rebase {
            assert_eq!(summary.predictions, 0, "{what}");
            assert_eq!(summary.final_fixups, 0, "{what}");
        }
        eprintln!("{what}: {summary:?}");
    }
    // Keep the capture's frames for agave-only statistics.
    let _ = child.slot();
    let _ = w.keys.len();
    let _ = ReadableAccount::lamports(&AccountSharedData::default());
}

/// Transactions of the chained child C that depend on P's freeze or on C's SlotHashes, on
/// P's new blockhash, and on the durable nonce P advanced.
fn chained_transactions(
    w: &World,
    last_blockhash_p: Hash,
    hash_p: Hash,
    collector: Pubkey,
) -> Vec<VersionedTransaction> {
    let old_blockhash = w.parent.last_blockhash();
    let nonce_after_p = *DurableNonce::from_blockhash(&old_blockhash).as_hash();
    let mut txs = transactions_with(w, 12, last_blockhash_p, nonce_after_p);
    let mut extra = Vec::new();
    // Votes on P (needs P's bank hash in C's SlotHashes) and on slot 1.
    for (i, (voter, vote)) in w.voters.iter().enumerate() {
        let (slot, hash) = if i == 0 {
            (2, hash_p)
        } else {
            (1, w.parent.hash())
        };
        extra.push(Transaction::new(
            &[voter],
            Message::new(
                &[vote_instruction::tower_sync(
                    vote,
                    &voter.pubkey(),
                    TowerSync::new_from_slots(vec![slot], hash, None),
                )],
                Some(&voter.pubkey()),
            ),
            last_blockhash_p,
        ));
    }
    // A vote on P with a wrong hash: fails in both.
    extra.push(Transaction::new(
        &[&w.voters[1].0],
        Message::new(
            &[vote_instruction::tower_sync(
                &w.voters[1].1,
                &w.voters[1].0.pubkey(),
                TowerSync::new_from_slots(vec![2], Hash::new_unique(), None),
            )],
            Some(&w.voters[1].0.pubkey()),
        ),
        last_blockhash_p,
    ));
    // Into and out of P's fee collector (P's freeze deposits fees into it) and its leader.
    for (i, to) in [collector, w.leader.pubkey()].into_iter().enumerate() {
        extra.push(Transaction::new(
            &[&w.keys[5]],
            Message::new(
                &[system_instruction::transfer(
                    &w.keys[5].pubkey(),
                    &to,
                    7_777_777 + i as u64,
                )],
                Some(&w.keys[5].pubkey()),
            ),
            old_blockhash,
        ));
    }
    extra.push(Transaction::new(
        &[&w.leader],
        Message::new(
            &[system_instruction::transfer(
                &w.leader.pubkey(),
                &w.keys[6].pubkey(),
                3_333_333,
            )],
            Some(&w.leader.pubkey()),
        ),
        last_blockhash_p,
    ));
    // Interleave the extra transactions early, in the middle and late.
    for (i, tx) in extra.into_iter().enumerate() {
        let at = (i * 37 + 3).min(txs.len());
        txs.insert(at, VersionedTransaction::from(tx));
    }
    txs
}

#[test]
fn test_chained_differential_against_agave() {
    let w = world();
    // Agave's P (slot 2): executes P's transactions and reaches its last tick, unfrozen.
    let txs_p = transactions(&w, 11);
    let bank_p = Bank::new_from_parent_with_bank_forks(
        &w.bank_forks,
        w.parent.clone(),
        SlotLeader {
            id: w.leader.pubkey(),
            vote_address: w.leader_vote,
        },
        2,
    );
    let statuses_p = execute_serially(&bank_p, &txs_p);
    assert!(statuses_p.values().filter(|s| s.is_ok()).count() > 100);
    let mut tick_hash = bank_p.last_blockhash();
    loop {
        tick_hash = solana_sha256_hasher::hashv(&[tick_hash.as_ref(), &[7]]);
        bank_p.register_tick_for_test(&tick_hash);
        if bank_p.last_blockhash() == tick_hash {
            break;
        }
    }
    assert!(!bank_p.is_frozen());

    // FL's run of P over the frozen slot 1.
    let graph = Arc::new(RwLock::new(FlForkGraph::default()));
    graph.write().unwrap().set_parent(1, 0);
    let mut programs = ProgramCaches::new(graph);
    let owners = Arc::new(vec![TOKEN_PROGRAM]);
    let run_p = Arc::new(Run::new(7, 2, &w.parent, &mut programs, owners.clone()).unwrap());
    let batches = push_halves(&run_p, &txs_p);
    let (outcomes_p, summary_p) =
        drive(run_p.clone(), 7, batches, 3, true, 0.5, false, None, None);
    assert_eq!(outcomes_p.len(), txs_p.len());
    assert_eq!(summary_p.unprocessable, 0);
    for outcome in &outcomes_p {
        assert_eq!(outcome.status, statuses_p[&outcome.signature]);
    }

    // The chained context is computed before agave freezes P ...
    let chained_run = |programs: &mut ProgramCaches| {
        Arc::new(
            Run::new_chained(8, 3, &run_p, &bank_p, tick_hash, programs, owners.clone()).unwrap(),
        )
    };
    let first_run_c = chained_run(&mut programs);
    // ... then agave freezes P, and C's transactions (votes on P's hash) exist.
    bank_p.freeze();
    let hash_p = bank_p.hash();
    let collector = bank_p.fast_lane_collector_id().unwrap();
    eprintln!(
        "P's fee collector {collector} (leader {}, vote {})",
        w.leader.pubkey(),
        w.leader_vote
    );
    let txs_c = chained_transactions(&w, tick_hash, hash_p, collector);
    let bank_c = Bank::new_from_parent_with_bank_forks(
        &w.bank_forks,
        bank_p.clone(),
        SlotLeader::new_unique(),
        3,
    );
    let statuses_c = execute_serially(&bank_c, &txs_c);
    let frames = w.capture.frames.lock().unwrap().clone();
    let vote_sig = txs_c
        .iter()
        .find(|tx| {
            tx.message.static_account_keys().contains(&w.voters[0].1)
        })
        .unwrap()
        .signatures[0];
    assert_eq!(statuses_c[&vote_sig], Ok(()), "agave: the vote on P succeeds");
    assert!(statuses_c.values().filter(|s| s.is_err()).count() > 3);

    let slot_hashes_id = solana_sdk_ids::sysvar::slot_hashes::id();
    let clock_id = solana_sdk_ids::sysvar::clock::id();
    for (i, (workers, speculation, theta, after_finals, rebase)) in [
        (1usize, false, 0.2f32, 0usize, false),
        (2, false, 0.2, usize::MAX, false),
        (3, true, 0.5, 40, false),
        (6, true, 1000.0, usize::MAX, false),
        (3, true, 0.2, usize::MAX, false),
        (3, true, 0.5, 40, true),
        (8, true, 1000.0, usize::MAX, true),
    ]
    .into_iter()
    .enumerate()
    {
        let what = format!(
            "chained K={workers} spec={speculation} theta={theta} resolve@{after_finals} rebase={rebase}"
        );
        let run_c = if i == 0 {
            first_run_c.clone()
        } else {
            chained_run(&mut programs)
        };
        assert!(run_c.ctx.is_chained());
        let mut batches = vec![(0, vec![run_c.provisional_meta().unwrap()])];
        batches.extend(push_halves(&run_c, &txs_c));
        let resolver = {
            let run_c = run_c.clone();
            let bank_p = bank_p.clone();
            External {
                after_finals,
                make: Box::new(move || {
                    let writes = run_c.resolve_parent_freeze(&bank_p).unwrap();
                    run_c.overlay.install(0, 0, &writes, &[]);
                    CoordMsg::ExternalDone {
                        run_id: 8,
                        k: 0,
                        out: ExecOutput {
                            reads: Vec::new(),
                            writes,
                            payload: Box::new(()),
                            unprocessable: false,
                            exec_start: Instant::now(),
                            exec_end: Instant::now(),
                        },
                    }
                }),
            }
        };
        let (outcomes, summary) = drive(
            run_c.clone(),
            8,
            batches,
            workers,
            speculation,
            theta,
            rebase,
            Some(resolver),
            None,
        );
        assert_eq!(outcomes.len(), txs_c.len(), "{what}: every tx final");
        assert_eq!(summary.unprocessable, 0, "{what}");
        for outcome in &outcomes {
            let sig = outcome.signature;
            assert!(outcome.chained);
            assert_eq!(&outcome.status, &statuses_c[&sig], "{what}: status of {sig}");
            assert_frames_equal(
                &format!("{what} tx {} {sig}", outcome.ordinal),
                &outcome.frame,
                frames.get(&(3, sig)),
            );
        }
        let mut ordinals: Vec<u32> = outcomes.iter().map(|o| o.ordinal).collect();
        ordinals.sort_unstable();
        assert_eq!(ordinals, (0..txs_c.len() as u32).collect::<Vec<_>>(), "{what}");
        // Final state (P's collector, SlotHistory, the resolved SlotHashes included).
        for key in run_c.overlay.written_keys() {
            if key == solana_sdk_ids::sysvar::slot_history::id() {
                continue; // C's own freeze has not happened.
            }
            let fl = run_c.overlay.latest(&key).value;
            let agave = bank_c.get_account(&key);
            assert!(same_value(&fl, &agave), "{what}: final state of {key}");
        }
        assert!(same_value(
            &run_c.overlay.latest(&slot_hashes_id).value,
            &bank_c.get_account(&slot_hashes_id)
        ));
        assert!(same_value(
            &Some(run_c.ctx.clock_account.clone()),
            &bank_c.get_account(&clock_id)
        ));
        if !speculation && after_finals == usize::MAX {
            // Both voters' first votes ran against the placeholder SlotHashes (they lock
            // no provisional key) and re-executed after the resolution (the wrong-hash vote
            // waits for voter 1's first vote and runs after it).
            assert!(summary.validation_failures >= 2, "{what}: {summary:?}");
        }
        eprintln!("{what}: {summary:?}");
    }
}

/// Phase 3: the output ring carries exactly agave's grouped notifications, filtered by
/// owner, one TX record per matching transaction, framed by SLOT_BEGIN / SLOT_END, with
/// per-account block order.
#[test]
fn test_output_ring_matches_agave_frames() {
    let w = world();
    let txs = transactions(&w, 11);
    let (_child, statuses) = run_agave(&w, &txs);
    let frames = w.capture.frames.lock().unwrap().clone();
    let owners = vec![system_program::id(), TOKEN_PROGRAM];

    for (workers, speculation, theta, rebase) in
        [(1usize, false, 0.2f32, false), (6, true, 1000.0, false), (8, true, 0.5, true)]
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fastlane.out.ring");
        let ring = OutRing::create(&path, 16 << 20).unwrap();
        let mut reader = OutRingReader::open(&path).unwrap();
        let stats = Arc::new(OutStats::default());
        let publisher = OutPublisher::new(ring, owners.clone(), stats.clone());

        let graph = Arc::new(RwLock::new(FlForkGraph::default()));
        graph.write().unwrap().set_parent(1, 0);
        let mut programs = ProgramCaches::new(graph);
        let run = Arc::new(
            Run::new(7, 2, &w.parent, &mut programs, Arc::new(vec![TOKEN_PROGRAM])).unwrap(),
        );
        let batches = push_halves(&run, &txs);
        let (outcomes, summary) =
            drive(run.clone(), 7, batches, workers, speculation, theta, rebase, None, Some(publisher));
        assert_eq!(outcomes.len(), txs.len());

        let mut records = Vec::new();
        loop {
            match reader.poll() {
                out_ring::Poll::Record(r) => records.push(*r),
                out_ring::Poll::Empty => break,
                out_ring::Poll::Reset => panic!("reset"),
            }
        }
        assert_eq!(records.first().unwrap().header.kind, out_ring::KIND_SLOT_BEGIN);
        let end = records.last().unwrap();
        assert_eq!(end.header.kind, out_ring::KIND_SLOT_END);
        assert_eq!(u64::from(end.header.tx_ordinal), summary.txs);
        let tx_records: Vec<_> = records
            .iter()
            .filter(|r| r.header.kind == out_ring::KIND_TX)
            .collect();
        // Expected: every agave frame with an account of a filtered owner.
        let expected: HashMap<Signature, Vec<(Pubkey, AccountSharedData)>> = frames
            .iter()
            .filter(|((slot, _), _)| *slot == 2)
            .filter_map(|((_, sig), frame)| {
                let kept: Vec<_> = frame
                    .iter()
                    .filter(|(_, a)| owners.contains(a.owner()))
                    .cloned()
                    .collect();
                (!kept.is_empty()).then(|| (*sig, kept))
            })
            .collect();
        assert!(expected.len() > 150);
        assert_eq!(tx_records.len(), expected.len(), "one record per matching tx");
        let mut last_writer: HashMap<Pubkey, u32> = HashMap::new();
        let mut readonly_seen = 0;
        for r in &tx_records {
            let sig = Signature::from(r.signature);
            let exp = &expected[&sig];
            assert_eq!(r.header.slot, 2);
            assert_eq!(r.header.parent_slot, 1);
            assert_eq!(r.header.fork_id, 7);
            assert_eq!(r.header.flags & out_ring::FLAG_OK != 0, statuses[&sig].is_ok());
            assert_eq!(r.accounts.len(), exp.len(), "{sig}");
            for (a, (key, b)) in r.accounts.iter().zip(exp) {
                assert_eq!(&a.pubkey, key);
                assert_eq!(&a.owner, b.owner());
                assert_eq!(a.lamports, b.lamports());
                assert_eq!(a.data, b.data());
                if a.flags & out_ring::ACCT_WRITTEN == 0 {
                    // Only the read-only token account is included without a write.
                    assert_eq!(a.pubkey, Pubkey::new_from_array([7; 32]));
                    readonly_seen += 1;
                } else {
                    // Per-account block order among writers.
                    if let Some(prev) = last_writer.insert(a.pubkey, r.header.tx_ordinal) {
                        assert!(prev < r.header.tx_ordinal, "block order of {}", a.pubkey);
                    }
                }
            }
            assert!(r.header.t_publish_ns >= r.header.t_source_ns);
        }
        assert!(readonly_seen > 0);
        assert_eq!(
            stats.tx_records.load(std::sync::atomic::Ordering::Relaxed) as usize,
            tx_records.len()
        );
        assert_eq!(
            stats.filtered.load(std::sync::atomic::Ordering::Relaxed) as usize,
            txs.len() - tx_records.len()
        );
    }
}
