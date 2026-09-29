#![cfg(feature = "agave-unstable-api")]
//! Execute-once (milestone 2, `commit = on`) tests.
//!
//! The same block (slot 2 over a frozen slot 1) is replayed by agave alone through the
//! unified scheduler (reference), and then, on a fresh bank of the same slot, through the
//! fast-lane commit path: FL executes the block and commits its results into agave's bank
//! (`commit_external`), while agave's unified scheduler replays the same entries as a
//! follower (`fast_lane_commit::follow`). The bank hash, every transaction status batch
//! (statuses, logs, inner instructions, fees, balances, costs, indexes) and every grouped
//! account notification must be identical.
//!
//! Variants: FL first then agave; both concurrently; agave racing for every cell (follow
//! wait 0); every transaction sampled (agave executes and compares); FL poisoned mid-slot;
//! and the replay-required outcomes (FL commits replay does not verify).
//!
//! The commit protocol is process-global (`fast_lane_commit` statics), so this file has a
//! single test driving every scenario in sequence.

use {
    agave_fast_lane::{
        FlHooks, ReplayServices,
        commit::{CommitEvent, CommitMetrics, Committer, commit_worker_loop},
        control,
        forks::FlForkGraph,
        program_cache::ProgramCaches,
        run::{Run, TxOutcome},
        sched::{CoordMsg, Coordinator, FinalSink, Finalized, RunId, RunSummary},
    },
    crossbeam_channel::{Receiver, unbounded},
    rand::{Rng, SeedableRng, rngs::StdRng},
    solana_account::AccountSharedData,
    solana_accounts_db::{
        accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING,
        accounts_update_notifier_interface::{AccountForGeyser, AccountsUpdateNotifierInterface},
    },
    solana_clock::{BankId, Slot},
    solana_entry::entry::Entry,
    solana_fee_structure::FeeStructure,
    solana_hash::Hash,
    solana_instruction::AccountMeta,
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
        fast_lane_commit::{self, STATS},
        genesis_utils::{GenesisConfigInfo, create_genesis_config_with_leader},
        installed_scheduler_pool::BankWithScheduler,
        runtime_config::RuntimeConfig,
        transaction_execution::{TransactionStatusMessage, TransactionStatusSender},
    },
    solana_signature::Signature,
    solana_signer::Signer,
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_transaction::{
        Transaction, TransactionVerificationMode, versioned::VersionedTransaction,
    },
    solana_unified_scheduler_pool::DefaultSchedulerPool,
    std::{
        collections::HashMap,
        sync::{
            Arc, Mutex, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    },
};

const TOKEN_PROGRAM: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SLOT: Slot = 2;

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
            accounts.iter().map(|(k, a)| (**k, (*a).clone())).collect(),
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
    /// Leader of slot 2 (its vote account is staked: the freeze deposits fees to its
    /// collector).
    leader: SlotLeader,
    parent: Arc<Bank>,
    bank_forks: Arc<RwLock<BankForks>>,
    capture: Arc<Capture>,
    tss: TransactionStatusSender,
    tss_rx: Receiver<TransactionStatusMessage>,
    keys: Vec<Keypair>,
    payers: Vec<Keypair>,
    nonce: Pubkey,
    nonce_authority: Keypair,
    nonce_hash: Hash,
}

fn world() -> World {
    world_with(3, 12)
}

fn world_with(handlers: usize, n_keys: usize) -> World {
    let leader = Keypair::new();
    let GenesisConfigInfo {
        genesis_config,
        voting_keypair,
        ..
    } = create_genesis_config_with_leader(
        1_000_000_000_000_000,
        &leader.pubkey(),
        1_000_000_000_000,
    );
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
    let parent = Bank::new_from_parent_with_bank_forks(&bank_forks, bank0, SlotLeader::default(), 1);
    let keys: Vec<Keypair> = (0..n_keys).map(|_| Keypair::new()).collect();
    let payers: Vec<Keypair> = (0..4).map(|_| Keypair::new()).collect();
    for (i, key) in keys.iter().enumerate() {
        let lamports = if i % 4 == 0 { 30_000 } else { 50_000_000_000 };
        parent.store_account(&key.pubkey(), &AccountSharedData::new(lamports, 0, &system_program::id()));
    }
    for payer in &payers {
        parent.store_account(
            &payer.pubkey(),
            &AccountSharedData::new(10_000_000_000, 0, &system_program::id()),
        );
    }
    parent.store_account(
        &Pubkey::new_from_array([7; 32]),
        &AccountSharedData::new(1_000_000, 82, &TOKEN_PROGRAM),
    );
    let nonce_authority = Keypair::new();
    let nonce = Pubkey::new_unique();
    let durable = DurableNonce::from_blockhash(&Hash::new_unique());
    let state = NonceState::new_initialized(&nonce_authority.pubkey(), durable, 5000);
    parent.store_account(
        &nonce,
        &AccountSharedData::new_data(1_000_000_000, &NonceVersions::new(state), &system_program::id())
            .unwrap(),
    );
    parent.store_account(
        &nonce_authority.pubkey(),
        &AccountSharedData::new(1_000_000_000, 0, &system_program::id()),
    );
    parent.freeze();
    let (sender, tss_rx) = unbounded();
    let tss = TransactionStatusSender {
        sender,
        dependency_tracker: None,
    };
    let pool = DefaultSchedulerPool::new(Some(handlers), None, Some(tss.clone()), None, None);
    bank_forks.write().unwrap().install_scheduler_pool(pool);
    World {
        leader: SlotLeader {
            id: leader.pubkey(),
            vote_address: voting_keypair.pubkey(),
        },
        parent,
        bank_forks,
        capture,
        tss,
        tss_rx,
        keys,
        payers,
        nonce,
        nonce_authority,
        nonce_hash: *durable.as_hash(),
    }
}

/// Conflicting transfers, payer chains, failures, account creation and drain, a durable
/// nonce, read-only token-owned accounts, and transactions mentioning a program loader
/// (never committed by FL: agave executes them among FL's commits).
fn transactions(w: &World, seed: u64, with_loader: bool) -> Vec<VersionedTransaction> {
    let mut rng = StdRng::seed_from_u64(seed);
    let blockhash = w.parent.last_blockhash();
    let token_ro = Pubkey::new_from_array([7; 32]);
    let mut txs = Vec::new();
    for i in 0..240u64 {
        let from = &w.keys[rng.random_range(0..w.keys.len())];
        let to = if rng.random_bool(0.1) {
            Pubkey::new_unique()
        } else {
            w.keys[rng.random_range(0..w.keys.len())].pubkey()
        };
        let lamports = match rng.random_range(0..10) {
            0 => 40_000_000_000 + i,
            1 => 1 + i,
            _ => rng.random_range(1_000..5_000_000) + i,
        };
        let mut ix = system_instruction::transfer(&from.pubkey(), &to, lamports);
        if rng.random_bool(0.2) {
            ix.accounts.push(AccountMeta::new_readonly(token_ro, false));
        }
        if with_loader && i % 37 == 5 {
            // Mentions the upgradeable loader: excluded from FL commit.
            ix.accounts.push(AccountMeta::new_readonly(
                solana_sdk_ids::bpf_loader_upgradeable::id(),
                false,
            ));
        }
        let tx = if rng.random_bool(0.5) {
            let payer = &w.payers[rng.random_range(0..w.payers.len())];
            Transaction::new(&[payer, from], Message::new(&[ix], Some(&payer.pubkey())), blockhash)
        } else {
            Transaction::new(&[from], Message::new(&[ix], Some(&from.pubkey())), blockhash)
        };
        txs.push(VersionedTransaction::from(tx));
        if i == 120 {
            let victim = &w.keys[1];
            txs.push(VersionedTransaction::from(Transaction::new(
                &[&w.payers[0], victim],
                Message::new(
                    &[system_instruction::transfer(&victim.pubkey(), &w.keys[2].pubkey(), 50_000_000_000)],
                    Some(&w.payers[0].pubkey()),
                ),
                blockhash,
            )));
        }
        if i == 150 {
            txs.push(VersionedTransaction::from(Transaction::new(
                &[&w.nonce_authority],
                Message::new(
                    &[
                        system_instruction::advance_nonce_account(&w.nonce, &w.nonce_authority.pubkey()),
                        system_instruction::transfer(&w.nonce_authority.pubkey(), &w.keys[3].pubkey(), 12_345),
                    ],
                    Some(&w.nonce_authority.pubkey()),
                ),
                w.nonce_hash,
            )));
        }
    }
    txs
}

fn entries_of(txs: &[VersionedTransaction]) -> Vec<Entry> {
    txs.chunks(8)
        .map(|chunk| Entry {
            num_hashes: 1,
            hash: Hash::default(),
            transactions: chunk.to_vec(),
        })
        .collect()
}

/// What a replay produced: bank hash, per-signature status batch contents, frames.
#[derive(Debug, PartialEq)]
struct Replayed {
    hash: Hash,
    statuses: HashMap<Signature, String>,
    frames: HashMap<Signature, Vec<(Pubkey, AccountSharedData)>>,
}

fn drain_statuses(w: &World) -> HashMap<Signature, String> {
    let mut out = HashMap::new();
    for msg in w.tss_rx.try_iter() {
        let TransactionStatusMessage::Batch((batch, _)) = msg else {
            continue;
        };
        for (i, tx) in batch.transactions.iter().enumerate() {
            let entry = format!(
                "index {:?} result {:?} pre {:?} post {:?} token_pre {:?} token_post {:?} cost {:?}",
                batch.transaction_indexes[i],
                batch.commit_results[i],
                batch.balances.pre_balances[i],
                batch.balances.post_balances[i],
                batch.token_balances.pre_token_balances[i],
                batch.token_balances.post_token_balances[i],
                batch.costs[i],
            );
            assert!(
                out.insert(*tx.signature(), entry).is_none(),
                "one status batch per transaction ({})",
                tx.signature()
            );
        }
    }
    out
}

fn new_bank(w: &World) -> BankWithScheduler {
    let bank = Bank::new_from_parent(w.parent.clone(), w.leader, SLOT);
    w.bank_forks.write().unwrap().insert(bank)
}

fn finish(w: &World, bank: BankWithScheduler) -> Replayed {
    bank.freeze();
    let hash = bank.hash();
    let statuses = drain_statuses(w);
    let frames = std::mem::take(&mut *w.capture.frames.lock().unwrap())
        .into_iter()
        .map(|((_, sig), frame)| (sig, frame))
        .collect();
    drop(bank);
    w.bank_forks.write().unwrap().clear_bank(SLOT, false);
    Replayed {
        hash,
        statuses,
        frames,
    }
}

/// Agave's replay of `txs` as transactions `0..` of the slot through the bank's unified
/// scheduler, as `confirm_slot` schedules them (task id = index in the slot), then wait.
/// (`process_entries_for_tests` numbers tasks from `Bank::transaction_count`, which FL's
/// commits advance.)
fn replay(
    bank: &BankWithScheduler,
    txs: &[VersionedTransaction],
) -> solana_transaction_error::TransactionResult<()> {
    let rtxs = verified(bank, txs);
    replay_verified(bank, rtxs)
}

fn verified(
    bank: &Bank,
    txs: &[VersionedTransaction],
) -> Vec<solana_runtime_transaction::runtime_transaction::RuntimeTransaction<
    solana_transaction::sanitized::SanitizedTransaction,
>> {
    txs.iter()
        .map(|tx| {
            bank.verify_transaction(tx.clone(), TransactionVerificationMode::FullVerification)
                .unwrap()
        })
        .collect()
}

fn replay_verified(
    bank: &BankWithScheduler,
    rtxs: Vec<
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction<
            solana_transaction::sanitized::SanitizedTransaction,
        >,
    >,
) -> solana_transaction_error::TransactionResult<()> {
    let scheduled = bank.schedule_transaction_executions(
        rtxs.into_iter()
            .enumerate()
            .map(|(i, rtx)| (rtx, i as u128)),
    );
    let waited = bank
        .wait_for_completed_scheduler()
        .map_or(Ok(()), |(result, _timings)| result);
    scheduled.and(waited)
}

fn replay_agave(w: &World, txs: &[VersionedTransaction]) -> Replayed {
    let bank = new_bank(w);
    replay(&bank, txs).unwrap();
    finish(w, bank)
}

struct TestSink {
    committer: Committer,
    finals: Arc<AtomicU64>,
    ended: Arc<AtomicBool>,
}

impl FinalSink for TestSink {
    fn on_final(&mut self, f: Finalized) {
        if let Ok(mut outcome) = f.payload.downcast::<TxOutcome>() {
            self.committer.on_final(
                f.run_id,
                f.k,
                &f.cpreds,
                f.cpred_writers,
                f.isolated,
                &mut outcome,
                f.t_final,
            );
            self.finals.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn on_run_end(&mut self, run_id: RunId, summary: RunSummary) {
        self.committer.on_run_end(run_id, &summary);
        self.ended.store(true, Ordering::SeqCst);
    }
    fn tick(&mut self) {
        self.committer.tick();
    }
    fn on_event(&mut self, event: Box<dyn std::any::Any + Send>) {
        if let Ok(event) = event.downcast::<CommitEvent>() {
            self.committer.on_event(*event);
        }
    }
}

/// A running fast lane (coordinator, executors, commit threads) on one run of slot 2.
struct Fl {
    exit: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
    metrics: Arc<CommitMetrics>,
    finals: Arc<AtomicU64>,
}

impl Fl {
    fn stop(self) {
        self.exit.store(true, Ordering::SeqCst);
        for t in self.threads {
            t.join().unwrap();
        }
    }
    fn declined(&self) -> u64 {
        self.metrics
            .declined
            .iter()
            .map(|d| d.load(Ordering::SeqCst))
            .sum()
    }
}

fn start_fl(w: &World, run_id: RunId, txs: &[VersionedTransaction], workers: usize) -> Fl {
    let graph = Arc::new(RwLock::new(FlForkGraph::default()));
    graph.write().unwrap().set_parent(1, 0);
    let mut programs = ProgramCaches::new(graph);
    let run = Arc::new(
        Run::new(run_id, SLOT, &w.parent, &mut programs, Arc::new(vec![TOKEN_PROGRAM])).unwrap(),
    );
    let exit = Arc::new(AtomicBool::new(false));
    let (task_tx, task_rx) = unbounded();
    let (coord_tx, coord_rx) = unbounded();
    let (job_tx, job_rx) = unbounded();
    let (side_tx, side_rx) = unbounded::<agave_fast_lane::sched::SideJob>();
    let metrics = Arc::new(CommitMetrics::default());
    let finals = Arc::new(AtomicU64::new(0));
    let ended = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for _ in 0..workers {
        let (task_rx, coord_tx, exit) = (task_rx.clone(), coord_tx.clone(), exit.clone());
        let side_rx = side_rx.clone();
        threads.push(
            thread::Builder::new()
                .name("tFlExec".into())
                .spawn(move || {
                    agave_fast_lane::sched::worker_loop_with_side(
                        task_rx,
                        Some(side_rx),
                        coord_tx,
                        exit,
                        Duration::from_micros(20),
                    )
                })
                .unwrap(),
        );
    }
    for _ in 0..3 {
        let (job_rx, coord_tx, exit, metrics) =
            (job_rx.clone(), coord_tx.clone(), exit.clone(), metrics.clone());
        let services = ReplayServices {
            transaction_status_sender: Some(w.tss.clone()),
            ..ReplayServices::default()
        };
        threads.push(
            thread::Builder::new()
                .name("tFlCommit".into())
                .spawn(move || {
                    commit_worker_loop(
                        job_rx,
                        coord_tx,
                        services,
                        exit,
                        Duration::from_micros(20),
                        metrics,
                    )
                })
                .unwrap(),
        );
    }
    fast_lane_commit::install_hooks(Arc::new(FlHooks {
        full_tx: crossbeam_channel::bounded(1).0,
        full_drops: Arc::default(),
        coord_tx: coord_tx.clone(),
    }));
    {
        let sink = TestSink {
            committer: Committer::new(job_tx, Some(w.bank_forks.clone()), metrics.clone())
                .with_workers(
                    side_tx,
                    coord_tx.clone(),
                    ReplayServices {
                        transaction_status_sender: Some(w.tss.clone()),
                        ..ReplayServices::default()
                    },
                ),
            finals: finals.clone(),
            ended,
        };
        let exit = exit.clone();
        threads.push(
            thread::Builder::new()
                .name("tFlCoord".into())
                .spawn(move || {
                    let tunables =
                        Arc::new(control::Tunables::new(true, true, 0.5, 3).with_rebase(true));
                    let mut coord = Coordinator::new(workers, task_tx, tunables, 1.0 / 32.0, sink);
                    coord.run_loop(coord_rx, exit, Duration::from_micros(20));
                })
                .unwrap(),
        );
    }
    coord_tx
        .send(CoordMsg::NewRun {
            run_id,
            run: run.clone(),
        })
        .unwrap();
    coord_tx
        .send(CoordMsg::Sink(Box::new(CommitEvent::RunBegin {
            run_id,
            run: run.clone(),
        })))
        .unwrap();
    let entries = entries_of(txs);
    let mut first = 0;
    for entry in entries {
        let (metas, failure) = run.push_entries(vec![entry], Instant::now(), 0, false);
        assert!(failure.is_none());
        let n = metas.len() as u32;
        coord_tx
            .send(CoordMsg::Txs {
                run_id,
                first,
                metas,
                t_ingest: Instant::now(),
            })
            .unwrap();
        first += n;
    }
    coord_tx
        .send(CoordMsg::InputComplete { run_id, total: first })
        .unwrap();
    Fl {
        exit,
        threads,
        metrics,
        finals,
    }
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Every transaction FINAL and FL's commits no longer progressing (the rest waits for agave:
/// declined transactions and their conflicting successors).
fn wait_fl_settled(fl: &Fl, board: &fast_lane_commit::Board, n: u64) {
    wait_until("FL finals", || fl.finals.load(Ordering::SeqCst) >= n);
    let mut last = u32::MAX;
    loop {
        let now = board.fl_committed();
        if now == last {
            return;
        }
        last = now;
        thread::sleep(Duration::from_millis(50));
    }
}

fn reset(mode: u8, ppm: u32, follow_wait_ms: u64) {
    fast_lane_commit::set_sample_mode(fast_lane_commit::SAMPLE_HASH);
    control::set_commit_on_workers(false);
    fast_lane_commit::set_bind_wait(Duration::from_millis(2));
    control::unpoison_for_tests();
    control::set_active(true);
    control::set_commit_mode(mode);
    control::set_verify_sample_ppm(ppm);
    control::set_follow_wait_ms(follow_wait_ms);
}

fn assert_same(what: &str, reference: &Replayed, got: &Replayed) {
    assert_eq!(reference.statuses.len(), got.statuses.len(), "{what}: status count");
    for (sig, status) in &reference.statuses {
        assert_eq!(Some(status), got.statuses.get(sig), "{what}: status batch of {sig}");
    }
    assert_eq!(reference.frames.len(), got.frames.len(), "{what}: frame count");
    for (sig, frame) in &reference.frames {
        assert_eq!(Some(frame), got.frames.get(sig), "{what}: frame of {sig}");
    }
    assert_eq!(reference.hash, got.hash, "{what}: bank hash");
}

#[test]
fn test_execute_once() {
    agave_fast_lane::control::set_agave_execution(control::AgaveExecution {
        record: true,
        log_messages_bytes_limit: None,
    });
    let w = world();

    // 0. A block without excluded transactions: FL alone commits all of it before agave's
    //    replay starts; agave's replay then only verifies and completes the tasks.
    let plain_txs = transactions(&w, 22, false);
    reset(control::COMMIT_OFF, 0, 100);
    let plain_reference = replay_agave(&w, &plain_txs);
    {
        let txs = plain_txs.clone();
        let n = txs.len() as u64;
        let reference = &plain_reference;
        reset(control::COMMIT_ON, 0, 100);
        let bank = new_bank(&w);
        let board = fast_lane_commit::board_of(bank.bank_id()).expect("commit board");
        let fl = start_fl(&w, 10, &txs, 4);
        wait_fl_settled(&fl, &board, n);
        assert_eq!(u64::from(board.fl_committed()), n, "FL committed the whole block alone");
        let follow_done = STATS.follow_done.load(Ordering::SeqCst);
        replay(&bank, &txs).unwrap();
        assert_eq!(board.agave_executed(), 0);
        assert_eq!(u64::from(board.verified()), n);
        assert_eq!(STATS.follow_done.load(Ordering::SeqCst) - follow_done, n);
        fast_lane_commit::close_bank(&bank).unwrap();
        fl.stop();
        let got = finish(&w, bank);
        assert_same("fl alone", reference, &got);
        eprintln!("fl alone: {n} committed by FL");
    }

    let txs = transactions(&w, 21, true);
    let n = txs.len() as u64;

    // Reference: agave alone (no fast lane, no board).
    reset(control::COMMIT_OFF, 0, 100);
    let reference = replay_agave(&w, &txs);
    assert_eq!(reference.statuses.len() as u64, n);
    let failures = reference
        .statuses
        .values()
        .filter(|s| !s.contains("status: Ok(())"))
        .count();
    assert!(failures > 5, "the block must contain failed transactions ({failures})");

    // 1. FL first: FL commits everything it may before agave's replay starts.
    reset(control::COMMIT_ON, 0, 100);
    let bank = new_bank(&w);
    let board = fast_lane_commit::board_of(bank.bank_id()).expect("commit board");
    let fl = start_fl(&w, 1, &txs, 4);
    wait_fl_settled(&fl, &board, n);
    let declined = fl.declined();
    let before_agave = board.fl_committed();
    assert!(declined >= 6, "loader transactions declined ({declined})");
    assert!(before_agave > 0, "FL committed {before_agave} alone");
    replay(&bank, &txs).unwrap();
    assert_eq!(u64::from(board.fl_committed()) + u64::from(board.agave_executed()), n);
    assert_eq!(board.verified(), board.fl_committed());
    assert_eq!(u64::from(board.agave_executed()), declined);
    assert!(
        fl.metrics.blockers[3].load(Ordering::SeqCst) > 0,
        "commits waited for agave's execution of declined (loader) transactions"
    );
    assert_eq!(
        fl.metrics.latency_us.lock().unwrap().len(),
        board.fl_committed() as usize,
        "every committed transaction attributed"
    );
    fast_lane_commit::close_bank(&bank).unwrap();
    fl.stop();
    let got = finish(&w, bank);
    assert_same("fl first", &reference, &got);
    eprintln!(
        "fl first: {} committed by FL ({before_agave} before agave's replay started), \
         {declined} by agave",
        board.fl_committed()
    );

    // 2. Concurrent: agave's replay starts together with FL and follows it.
    for (label, follow_wait_ms, workers) in [("concurrent", 100, 4), ("race", 0, 2), ("race8", 0, 8)] {
        reset(control::COMMIT_ON, 0, follow_wait_ms);
        let bank = new_bank(&w);
        let board = fast_lane_commit::board_of(bank.bank_id()).unwrap();
        let fl = start_fl(&w, 2, &txs, workers);
        replay(&bank, &txs).unwrap();
        assert_eq!(
            u64::from(board.fl_committed()) + u64::from(board.agave_executed()),
            n,
            "{label}: every transaction committed exactly once"
        );
        assert_eq!(board.verified(), board.fl_committed(), "{label}");
        fast_lane_commit::close_bank(&bank).unwrap();
        fl.stop();
        let (by_fl, by_agave) = (board.fl_committed(), board.agave_executed());
        let got = finish(&w, bank);
        assert_same(label, &reference, &got);
        eprintln!("{label}: {by_fl} committed by FL, {by_agave} by agave");
    }

    // 3. Every transaction sampled: agave executes all and compares with FL's results.
    let mismatches = STATS.sample_mismatches.load(Ordering::SeqCst);
    let samples = STATS.samples.load(Ordering::SeqCst);
    let sample_timeouts = STATS.sample_timeouts.load(Ordering::SeqCst);
    reset(control::COMMIT_ON, 1_000_000, 100);
    let bank = new_bank(&w);
    let board = fast_lane_commit::board_of(bank.bank_id()).unwrap();
    assert_eq!(board.sample_ppm, 1_000_000);
    let fl = start_fl(&w, 3, &txs, 4);
    wait_until("FL deposits", || fl.finals.load(Ordering::SeqCst) >= n);
    replay(&bank, &txs).unwrap();
    assert_eq!(board.fl_committed(), 0);
    assert_eq!(u64::from(board.agave_executed()), n);
    assert_eq!(STATS.samples.load(Ordering::SeqCst) - samples, n);
    assert_eq!(
        STATS.sample_timeouts.load(Ordering::SeqCst),
        sample_timeouts,
        "FL deposited every sampled result (declined ones included)"
    );
    assert_eq!(STATS.sample_mismatches.load(Ordering::SeqCst), mismatches, "no sample differs");
    assert!(!control::is_poisoned());
    fast_lane_commit::close_bank(&bank).unwrap();
    fl.stop();
    let got = finish(&w, bank);
    assert_same("sampled", &reference, &got);

    // 3b. FL-picked samples (`sample_mode = fl`): FL samples only transactions no later
    //     transaction conflicts with; the rest is committed by FL; no sample differs.
    reset(control::COMMIT_ON, 1_000_000, 100);
    fast_lane_commit::set_sample_mode(fast_lane_commit::SAMPLE_FL);
    let samples = STATS.samples.load(Ordering::SeqCst);
    let sample_timeouts = STATS.sample_timeouts.load(Ordering::SeqCst);
    let bank = new_bank(&w);
    let board = fast_lane_commit::board_of(bank.bank_id()).unwrap();
    let fl = start_fl(&w, 30, &txs, 4);
    wait_fl_settled(&fl, &board, n);
    replay(&bank, &txs).unwrap();
    let picked = fl.metrics.samples_picked.load(Ordering::SeqCst);
    assert!(picked > 0, "FL picked samples");
    assert!(u64::from(board.fl_committed()) + picked + declined >= n - 2);
    assert_eq!(STATS.samples.load(Ordering::SeqCst) - samples, picked);
    assert_eq!(STATS.sample_timeouts.load(Ordering::SeqCst), sample_timeouts);
    assert_eq!(STATS.sample_mismatches.load(Ordering::SeqCst), mismatches);
    assert_eq!(
        fl.metrics.blockers[2].load(Ordering::SeqCst),
        0,
        "no commit waited for a picked sample"
    );
    fast_lane_commit::close_bank(&bank).unwrap();
    fl.stop();
    let got = finish(&w, bank);
    assert_same("fl-picked samples", &reference, &got);
    eprintln!("fl-picked samples: {picked} of {n}, {} committed by FL", board.fl_committed());

    // 3c. Commits on FL's executor threads (`commit_on = workers`), concurrent with agave.
    reset(control::COMMIT_ON, 0, 100);
    control::set_commit_on_workers(true);
    let bank = new_bank(&w);
    let board = fast_lane_commit::board_of(bank.bank_id()).unwrap();
    let fl = start_fl(&w, 31, &txs, 4);
    replay(&bank, &txs).unwrap();
    assert_eq!(u64::from(board.fl_committed()) + u64::from(board.agave_executed()), n);
    assert_eq!(
        fl.metrics.on_workers.load(Ordering::SeqCst),
        u64::from(board.fl_committed()),
        "every FL commit ran on an executor thread"
    );
    fast_lane_commit::close_bank(&bank).unwrap();
    fl.stop();
    let got = finish(&w, bank);
    assert_same("commit on workers", &reference, &got);

    // 3d. Bank inserted after FL started its run: agave's replay waits for FL's binding
    //     instead of executing (no unbound agave executions).
    reset(control::COMMIT_ON, 0, 100);
    fast_lane_commit::set_bind_wait(Duration::from_millis(50));
    let unbound = STATS.agave_unbound.load(Ordering::SeqCst);
    let bind_waits = STATS.bind_waits.load(Ordering::SeqCst);
    let fl = start_fl(&w, 32, &txs, 4);
    thread::sleep(Duration::from_millis(50)); // the run begins, its expectation is recorded
    let bank = new_bank(&w);
    let board = fast_lane_commit::board_of(bank.bank_id()).unwrap();
    replay(&bank, &txs).unwrap();
    assert_eq!(u64::from(board.fl_committed()) + u64::from(board.agave_executed()), n);
    assert_eq!(
        STATS.agave_unbound.load(Ordering::SeqCst),
        unbound,
        "agave waited for the binding (bind waits {})",
        STATS.bind_waits.load(Ordering::SeqCst) - bind_waits
    );
    fast_lane_commit::close_bank(&bank).unwrap();
    fl.stop();
    let got = finish(&w, bank);
    assert_same("bind wait", &reference, &got);

    // 4. FL poisoned mid-slot: FL has committed part of the block; agave executes the rest
    //    at once (no waiting), and the bank is still exact (FL's commits were valid).
    //    (The loader-free block, so that FL alone gets past the first few transactions.)
    reset(control::COMMIT_ON, 0, 60_000);
    let bank = new_bank(&w);
    let board = fast_lane_commit::board_of(bank.bank_id()).unwrap();
    let fl = start_fl(&w, 4, &plain_txs, 1);
    let deadline = Instant::now() + Duration::from_secs(60);
    while board.fl_committed() < 5 {
        assert!(Instant::now() < deadline, "no FL commits");
        std::hint::spin_loop();
    }
    control::poison("test: poison mid-slot");
    assert!(!fast_lane_commit::fl_live());
    let t0 = Instant::now();
    replay(&bank, &plain_txs).unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(20),
        "agave did not wait for a poisoned FL (follow wait is 60 s)"
    );
    let (by_fl, by_agave) = (board.fl_committed(), board.agave_executed());
    assert!(by_fl > 0 && by_agave > 0, "{by_fl} / {by_agave}");
    assert_eq!(u64::from(by_fl) + u64::from(by_agave), plain_txs.len() as u64);
    fast_lane_commit::close_bank(&bank).unwrap();
    fl.stop();
    let got = finish(&w, bank);
    assert_same("poison mid-slot", &plain_reference, &got);
    eprintln!("poison mid-slot: {by_fl} committed by FL, {by_agave} by agave");

    // 5. FL committed transactions replay does not have (e.g. another block version): replay
    //    of the first half only; closing the bank requires a replay without FL.
    reset(control::COMMIT_ON, 0, 100);
    let bank = new_bank(&w);
    let board = fast_lane_commit::board_of(bank.bank_id()).unwrap();
    let fl = start_fl(&w, 5, &txs, 4);
    wait_fl_settled(&fl, &board, n);
    replay(&bank, &txs[..txs.len() / 4]).unwrap();
    assert!(board.verified() < board.fl_committed());
    assert_eq!(fast_lane_commit::close_bank(&bank), Err("unverified_commit"));
    assert!(control::is_poisoned(), "an unverified commit poisons FL");
    fl.stop();
    drop(bank);
    let _ = drain_statuses(&w);
    w.capture.frames.lock().unwrap().clear();
    w.bank_forks.write().unwrap().clear_bank(SLOT, false);

    // 6. After the slot is marked for replay without FL, a new bank of it gets no board, and
    //    agave alone reproduces the reference.
    fast_lane_commit::mark_agave_only(SLOT);
    reset(control::COMMIT_ON, 0, 100);
    let bank = new_bank(&w);
    assert!(fast_lane_commit::board_of(bank.bank_id()).is_none());
    replay(&bank, &txs).unwrap();
    let got = finish(&w, bank);
    assert_same("agave-only retry", &reference, &got);

    fast_lane_commit::uninstall_hooks();
}

/// CPU nanoseconds used so far by this process's threads whose name starts with `prefix`
/// (Linux `schedstat`; 0 elsewhere).
fn thread_cpu_ns(prefix: &str) -> u64 {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    dir.filter_map(|e| e.ok())
        .filter(|e| {
            std::fs::read_to_string(e.path().join("comm"))
                .is_ok_and(|comm| comm.trim().starts_with(prefix))
        })
        .filter_map(|e| std::fs::read_to_string(e.path().join("schedstat")).ok())
        .filter_map(|stat| stat.split_whitespace().next()?.parse::<u64>().ok())
        .sum()
}

/// Many independent transfers plus a few hot chains (a payer used by every 20th
/// transaction), roughly a busy mainnet block's shape at system-transfer cost.
fn bench_transactions(w: &World, n: usize, seed: u64) -> Vec<VersionedTransaction> {
    let mut rng = StdRng::seed_from_u64(seed);
    let blockhash = w.parent.last_blockhash();
    (0..n)
        .map(|i| {
            let from = &w.keys[rng.random_range(4..w.keys.len())];
            let to = w.keys[rng.random_range(4..w.keys.len())].pubkey();
            let ix = system_instruction::transfer(&from.pubkey(), &to, 1_000 + i as u64);
            let tx = if i % 20 == 0 {
                let payer = &w.payers[i % w.payers.len()];
                Transaction::new(&[payer, from], Message::new(&[ix], Some(&payer.pubkey())), blockhash)
            } else {
                Transaction::new(&[from], Message::new(&[ix], Some(&from.pubkey())), blockhash)
            };
            VersionedTransaction::from(tx)
        })
        .collect()
}

/// Milestone 3 measurements (run on FRA with `--ignored --nocapture`): replay of a
/// 4,000-transaction block with 2 and 7 unified-scheduler handlers, agave alone (the fallback
/// case) and in commit mode (FL commits, agave follows): wall time and CPU of agave's
/// handler and scheduler threads, and of FL's threads.
#[test]
#[ignore]
fn bench_handlers() {
    control::set_agave_execution(control::AgaveExecution {
        record: true,
        log_messages_bytes_limit: None,
    });
    for handlers in [7usize, 2] {
        let w = world_with(handlers, 3_000);
        let txs = bench_transactions(&w, 4_000, 5);
        let n = txs.len() as u64;
        for round in 0..3 {
            // Agave alone.
            reset(control::COMMIT_OFF, 0, 100);
            let (h0, s0) = (thread_cpu_ns("solScHandle"), thread_cpu_ns("solScheduleV"));
            let bank = new_bank(&w);
            let rtxs = verified(&bank, &txs);
            let t0 = Instant::now();
            replay_verified(&bank, rtxs).unwrap();
            let agave_wall = t0.elapsed();
            let (h1, s1) = (thread_cpu_ns("solScHandle"), thread_cpu_ns("solScheduleV"));
            let _ = finish(&w, bank);
            // Commit mode: FL and agave start together.
            reset(control::COMMIT_ON, 0, 100);
            let bank = new_bank(&w);
            let board = fast_lane_commit::board_of(bank.bank_id()).unwrap();
            let (fe0, fc0, fo0) = (
                thread_cpu_ns("tFlExec"),
                thread_cpu_ns("tFlCommit"),
                thread_cpu_ns("tFlCoord"),
            );
            let rtxs = verified(&bank, &txs);
            let t0 = Instant::now();
            let fl = start_fl(&w, 100 + round, &txs, 8);
            let (h2, s2) = (thread_cpu_ns("solScHandle"), thread_cpu_ns("solScheduleV"));
            replay_verified(&bank, rtxs).unwrap();
            let commit_wall = t0.elapsed();
            let (h3, s3) = (thread_cpu_ns("solScHandle"), thread_cpu_ns("solScheduleV"));
            let (fe1, fc1, fo1) = (
                thread_cpu_ns("tFlExec"),
                thread_cpu_ns("tFlCommit"),
                thread_cpu_ns("tFlCoord"),
            );
            let by_fl = board.fl_committed();
            fast_lane_commit::close_bank(&bank).unwrap();
            fl.stop();
            let _ = finish(&w, bank);
            let ms = |ns: u64| ns as f64 / 1e6;
            eprintln!(
                "handlers={handlers} round={round} n={n}: agave alone wall {:.1} ms, handler cpu \
                 {:.1} ms, scheduler cpu {:.1} ms | commit mode wall {:.1} ms (FL committed \
                 {by_fl}), handler cpu {:.1} ms, scheduler cpu {:.1} ms, FL exec cpu {:.1} ms, \
                 FL commit cpu {:.1} ms, FL coordinator cpu {:.1} ms",
                agave_wall.as_secs_f64() * 1e3,
                ms(h1 - h0),
                ms(s1 - s0),
                commit_wall.as_secs_f64() * 1e3,
                ms(h3 - h2),
                ms(s3 - s2),
                ms(fe1 - fe0),
                ms(fc1 - fc0),
                ms(fo1 - fo0),
            );
        }
    }
}
