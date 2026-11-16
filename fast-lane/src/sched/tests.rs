//! Property tests: the scheduler over a synthetic deterministic state machine must produce
//! exactly the serial result, emit each account's writers in block order, never deadlock,
//! and respect the incarnation cap — for random DAGs, worker counts, delays, hint
//! thresholds, with and without speculation and eager re-execution.

use {
    super::*,
    crate::mv::{BaseReader, Overlay},
    rand::{Rng, SeedableRng, rngs::StdRng},
    solana_account::{ReadableAccount, WritableAccount},
    solana_clock::Slot,
    std::{
        collections::HashMap,
        sync::Mutex as StdMutex,
        thread,
    },
};

struct MapBase(HashMap<Pubkey, AccountSharedData>);
impl BaseReader for MapBase {
    fn read(&self, key: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        self.0.get(key).cloned().map(|a| (a, 1))
    }
}

fn fnv(bytes: impl IntoIterator<Item = u8>, seed: u64) -> u64 {
    let mut h = 0xcbf29ce484222325u64 ^ seed;
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Clone)]
struct MockTx {
    locks: Vec<(Pubkey, bool)>,
    /// Commutative accounts ("fee sinks"): a write-locker credits them by an amount that
    /// depends only on the transaction, and their value does not influence anything else.
    /// With any sinks, the fee payer's balance does not influence the outcome either (a fee
    /// payer with enough funds). Empty = the original fully value-dependent semantics.
    sinks: Arc<Vec<Pubkey>>,
}

/// Deterministic semantics of transaction `k` given the values it read, in lock order.
fn mock_semantics(
    k: TxIdx,
    tx: &MockTx,
    reads: &[Option<AccountSharedData>],
) -> Vec<(Pubkey, AccountSharedData)> {
    let commutative = !tx.sinks.is_empty();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&k.to_le_bytes());
    for (i, r) in reads.iter().enumerate() {
        if commutative && (i == 0 || tx.sinks.contains(&tx.locks[i].0)) {
            continue;
        }
        match r {
            Some(a) => {
                bytes.extend_from_slice(&a.lamports().to_le_bytes());
                bytes.extend_from_slice(a.data());
            }
            None => bytes.push(0xff),
        }
    }
    let h = fnv(bytes, 7);
    let fee_payer = tx.locks[0].0;
    let payer_value = |reads: &[Option<AccountSharedData>]| {
        let mut a = reads[0].clone().unwrap_or_else(|| {
            AccountSharedData::new(1_000_000, 8, &Pubkey::new_from_array([1; 32]))
        });
        a.set_lamports(a.lamports().saturating_sub(1).max(1));
        a
    };
    if h % 5 == 0 {
        // "Failed": only the fee payer is written.
        return vec![(fee_payer, payer_value(reads))];
    }
    let mut writes = vec![(fee_payer, payer_value(reads))];
    for (i, (key, w)) in tx.locks.iter().enumerate().skip(1) {
        if !*w {
            continue;
        }
        if tx.sinks.contains(key) {
            // Credit: lamports and the data word, by amounts that depend only on k.
            let mut a = reads[i].clone().unwrap_or_else(|| {
                AccountSharedData::new(10, 8, &Pubkey::new_from_array([1; 32]))
            });
            a.set_lamports(a.lamports() + u64::from(k % 7) + 1);
            let word = u64::from_le_bytes(a.data()[..8].try_into().unwrap());
            a.data_as_mut_slice()[..8]
                .copy_from_slice(&word.wrapping_add(u64::from(k % 11) * 3 + 1).to_le_bytes());
            writes.push((*key, a));
            continue;
        }
        let mode = (h >> (i * 2)) % 8;
        let old = reads[i].clone();
        match mode {
            // Not touched.
            0..=3 => {}
            // Touched but byte-identical.
            4 | 5 => {
                if let Some(old) = old {
                    writes.push((*key, old));
                }
            }
            // Changed.
            _ => {
                let mut a = old.unwrap_or_else(|| {
                    AccountSharedData::new(10, 8, &Pubkey::new_from_array([1; 32]))
                });
                a.set_lamports(a.lamports().wrapping_add(h % 97 + 1));
                let d = fnv(h.to_le_bytes(), i as u64).to_le_bytes();
                a.data_as_mut_slice().copy_from_slice(&d);
                writes.push((*key, a));
            }
        }
    }
    writes
}

struct MockRun {
    overlay: Overlay,
    txs: Vec<MockTx>,
    max_delay_us: u64,
    seed: u64,
}

impl SchedRun for MockRun {
    fn overlay(&self) -> &Overlay {
        &self.overlay
    }
    fn execute(&self, k: TxIdx, inc: u32) -> ExecOutput {
        let exec_start = Instant::now();
        let tx = &self.txs[k as usize];
        let mut reads = Vec::with_capacity(tx.locks.len());
        let mut values = Vec::with_capacity(tx.locks.len());
        for (key, _) in &tx.locks {
            let vis = self.overlay.visible_below(key, k);
            values.push(vis.value.clone());
            reads.push(Read {
                key: *key,
                origin: vis.origin,
                value: vis.value,
            });
            if self.max_delay_us > 0 {
                let d = fnv(
                    [k.to_le_bytes(), inc.to_le_bytes()].concat(),
                    self.seed,
                ) % self.max_delay_us;
                thread::sleep(Duration::from_micros(d / 4));
            }
        }
        let writes = mock_semantics(k, tx, &values);
        ExecOutput {
            reads,
            payload: Box::new(writes.clone()),
            writes,
            unprocessable: false,
            exec_start,
            exec_end: Instant::now(),
        }
    }
}

#[derive(Default)]
struct CollectSink {
    finals: Vec<(TxIdx, u32, Vec<(Pubkey, AccountSharedData)>)>,
    ended: Vec<(RunId, RunSummary)>,
    /// Ingest -> FINAL per transaction (µs).
    latency_us: Vec<u64>,
}

impl FinalSink for Arc<StdMutex<CollectSink>> {
    fn on_final(&mut self, f: Finalized) {
        let writes = *f
            .payload
            .downcast::<Vec<(Pubkey, AccountSharedData)>>()
            .unwrap();
        let mut sink = self.lock().unwrap();
        sink.latency_us
            .push(f.t_final.saturating_duration_since(f.t_ingest).as_micros() as u64);
        sink.finals.push((f.k, f.incarnations, writes));
    }
    fn on_run_end(&mut self, run_id: RunId, summary: RunSummary) {
        self.lock().unwrap().ended.push((run_id, summary));
    }
}

struct Case {
    seed: u64,
    n_accounts: usize,
    n_txs: usize,
    workers: usize,
    speculation: bool,
    eager: bool,
    theta: f32,
    max_inc: u32,
    max_delay_us: u64,
    batch: usize,
    rebase: bool,
    /// Commutative sink accounts among the hot accounts (see [`MockTx::sinks`]).
    sinks: usize,
    /// Probability that a transaction uses one of a few shared fee payers.
    shared_payers: f64,
    /// Pause between fed batches (µs).
    feed_gap_us: u64,
    /// Start with learned hints for the sinks and shared payers (a warm coordinator, as in
    /// production where hints persist across slots).
    warm_hints: bool,
}

impl Default for Case {
    fn default() -> Self {
        Self {
            seed: 1,
            n_accounts: 10,
            n_txs: 100,
            workers: 3,
            speculation: true,
            eager: true,
            theta: 0.5,
            max_inc: 3,
            max_delay_us: 0,
            batch: 16,
            rebase: false,
            sinks: 0,
            shared_payers: 0.3,
            feed_gap_us: 100,
            warm_hints: false,
        }
    }
}

thread_local! {
    static LAST_LATENCIES: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn run_case(case: &Case) -> RunSummary {
    let mut rng = StdRng::seed_from_u64(case.seed);
    let keys: Vec<Pubkey> = (0..case.n_accounts).map(|_| Pubkey::new_unique()).collect();
    let mut base = HashMap::new();
    for key in &keys {
        if rng.random_bool(0.9) {
            let mut a = AccountSharedData::new(rng.random_range(1..1_000_000), 8, &Pubkey::new_from_array([1; 32]));
            a.data_as_mut_slice().copy_from_slice(&rng.random::<u64>().to_le_bytes());
            base.insert(*key, a);
        }
    }
    // Skewed account popularity: a few hot accounts.
    let pick = |rng: &mut StdRng| -> Pubkey {
        if rng.random_bool(0.5) {
            keys[rng.random_range(0..3.min(keys.len()))]
        } else {
            keys[rng.random_range(0..keys.len())]
        }
    };
    let sinks: Arc<Vec<Pubkey>> = Arc::new(keys.iter().take(case.sinks.min(keys.len().saturating_sub(1))).copied().collect());
    let mut txs = Vec::new();
    for _ in 0..case.n_txs {
        let n = rng.random_range(1..=5);
        let mut locks: Vec<(Pubkey, bool)> = Vec::new();
        // Fee payers: mostly distinct, sometimes shared (payer chains). With sinks, shared
        // payers are a few dedicated wallets (bots sending many transactions).
        let payer = if rng.random_bool(case.shared_payers) {
            if sinks.is_empty() {
                keys[rng.random_range(0..keys.len())]
            } else {
                Pubkey::new_from_array([200 + rng.random_range(0..3u8); 32])
            }
        } else {
            Pubkey::new_unique()
        };
        locks.push((payer, true));
        while locks.len() < n + 1 {
            let key = pick(&mut rng);
            if locks.iter().any(|(k, _)| *k == key) {
                if locks.len() >= keys.len() {
                    break;
                }
                continue;
            }
            if sinks.contains(&key) && key == payer {
                continue;
            }
            locks.push((key, rng.random_bool(0.6)));
        }
        txs.push(MockTx {
            locks,
            sinks: Arc::clone(&sinks),
        });
    }
    if !sinks.is_empty() {
        for i in 0..3u8 {
            base.insert(
                Pubkey::new_from_array([200 + i; 32]),
                AccountSharedData::new(1_000_000_000, 0, &Pubkey::default()),
            );
        }
    }

    // Serial reference.
    let mut state: HashMap<Pubkey, AccountSharedData> = base.clone();
    let mut expected = Vec::new();
    for (k, tx) in txs.iter().enumerate() {
        let values: Vec<Option<AccountSharedData>> = tx
            .locks
            .iter()
            .map(|(key, _)| state.get(key).filter(|a| a.lamports() != 0).cloned())
            .collect();
        let writes = mock_semantics(k as TxIdx, tx, &values);
        for (key, a) in &writes {
            state.insert(*key, a.clone());
        }
        expected.push(writes);
    }

    let run = Arc::new(MockRun {
        overlay: Overlay::new(2, Arc::new(MapBase(base))),
        txs: txs.clone(),
        max_delay_us: case.max_delay_us,
        seed: case.seed,
    });
    let (task_tx, task_rx) = crossbeam_channel::unbounded();
    let (coord_tx, coord_rx) = crossbeam_channel::unbounded();
    let exit = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..case.workers)
        .map(|_| {
            let task_rx = task_rx.clone();
            let coord_tx = coord_tx.clone();
            let exit = exit.clone();
            thread::spawn(move || worker_loop(task_rx, coord_tx, exit, Duration::from_micros(20)))
        })
        .collect();
    let sink = Arc::new(StdMutex::new(CollectSink::default()));
    let tunables = Arc::new(
        Tunables::new(case.speculation, case.eager, case.theta, case.max_inc)
            .with_rebase(case.rebase),
    );
    let mut coord = Coordinator::new(case.workers, task_tx, tunables, 0.25, Arc::clone(&sink));
    if case.warm_hints {
        let payers = (0..3u8).map(|i| Pubkey::new_from_array([200 + i; 32]));
        for key in sinks.iter().copied().chain(payers) {
            for _ in 0..64 {
                coord.hints.observe(&key, true);
                coord.hints.observe_miss(&key, false, false);
            }
        }
    }
    coord_tx
        .send(CoordMsg::NewRun {
            run_id: 1,
            run: run.clone(),
        })
        .unwrap();
    // Feed transactions in batches, interleaved with execution.
    let feeder = {
        let coord_tx = coord_tx.clone();
        let txs = txs.clone();
        let batch = case.batch;
        let gap = case.feed_gap_us;
        thread::spawn(move || {
            let mut first = 0usize;
            while first < txs.len() {
                let end = (first + batch).min(txs.len());
                let metas = txs[first..end]
                    .iter()
                    .map(|tx| TxMeta {
                        locks: tx.locks.clone(),
                        certain_writes: vec![tx.locks[0].0],
                        is_vote: false,
                        external: false,
                    })
                    .collect();
                coord_tx
                    .send(CoordMsg::Txs {
                        run_id: 1,
                        first: first as TxIdx,
                        metas,
                        t_ingest: Instant::now(),
                    })
                    .unwrap();
                first = end;
                thread::sleep(Duration::from_micros(gap));
            }
            coord_tx
                .send(CoordMsg::InputComplete {
                    run_id: 1,
                    total: txs.len() as TxIdx,
                })
                .unwrap();
        })
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(msg) = coord_rx.recv_timeout(Duration::from_millis(5)) {
            coord.handle(msg);
            while let Ok(msg) = coord_rx.try_recv() {
                coord.handle(msg);
            }
            coord.dispatch();
        }
        if !sink.lock().unwrap().ended.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "deadlock: states {:?}",
            coord.run_state_counts(1)
        );
    }
    feeder.join().unwrap();
    exit.store(true, Ordering::Relaxed);
    drop(coord);
    drop(coord_tx);
    for w in workers {
        w.join().unwrap();
    }

    let sink = sink.lock().unwrap();
    assert_eq!(sink.finals.len(), case.n_txs, "every tx final");
    let mut seen = vec![false; case.n_txs];
    for (k, incarnations, writes) in &sink.finals {
        assert!(!seen[*k as usize], "tx {k} finalized twice");
        seen[*k as usize] = true;
        let exp = &expected[*k as usize];
        assert_eq!(writes.len(), exp.len(), "tx {k} write count");
        for ((wk, wa), (ek, ea)) in writes.iter().zip(exp) {
            assert_eq!(wk, ek, "tx {k} write key");
            assert!(crate::mv::accounts_equal(wa, ea), "tx {k} value of {wk}");
        }
        assert!(
            *incarnations <= case.max_inc + 1,
            "tx {k}: {incarnations} incarnations > cap {}",
            case.max_inc
        );
    }
    // Final state equals serial state.
    for (key, a) in &state {
        let vis = run.overlay.latest(key);
        let expected_value = (a.lamports() != 0).then(|| a.clone());
        assert!(
            crate::mv::same_value(&vis.value, &expected_value),
            "final state of {key}"
        );
    }
    // Per-account block order of FINAL events among write-lockers.
    let order: HashMap<TxIdx, usize> = sink
        .finals
        .iter()
        .enumerate()
        .map(|(i, (k, _, _))| (*k, i))
        .collect();
    for key in &keys {
        let writers: Vec<TxIdx> = txs
            .iter()
            .enumerate()
            .filter(|(_, tx)| tx.locks.iter().any(|(k, w)| k == key && *w))
            .map(|(i, _)| i as TxIdx)
            .collect();
        for pair in writers.windows(2) {
            assert!(
                order[&pair[0]] < order[&pair[1]],
                "account {key}: writer {} emitted after {}",
                pair[0],
                pair[1]
            );
        }
    }
    LAST_LATENCIES.with(|l| *l.borrow_mut() = sink.latency_us.clone());
    let (_, summary) = &sink.ended[0];
    assert_eq!(summary.finals as usize, case.n_txs);
    assert!(summary.aborted.is_none());
    if !(case.rebase && case.eager) {
        assert_eq!(summary.predictions + summary.shadow_predictions, 0, "{summary:?}");
        assert_eq!(summary.final_fixups, 0, "{summary:?}");
        assert_eq!(summary.spec_relaxed, 0, "{summary:?}");
    }
    summary.clone()
}

#[test]
fn test_serial_equivalence_many_cases() {
    let mut rng = StdRng::seed_from_u64(42);
    for i in 0..60u64 {
        let case = Case {
            seed: 1000 + i,
            n_accounts: rng.random_range(3..40),
            n_txs: rng.random_range(1..300),
            workers: [1usize, 2, 3, 6][rng.random_range(0..4)],
            speculation: rng.random_bool(0.8),
            eager: rng.random_bool(0.5),
            theta: [0.0f32, 0.2, 0.5, 5.0, 1000.0][rng.random_range(0..5)],
            max_inc: rng.random_range(1..4),
            max_delay_us: [0u64, 50, 400][rng.random_range(0..3)],
            batch: rng.random_range(1..64),
            ..Case::default()
        };
        let _ = run_case(&case);
    }
}

#[test]
fn test_blind_speculation_hot_account() {
    // Everything on 2 hot accounts, blind speculation, many workers: maximum conflicts.
    let summary = run_case(&Case {
        seed: 7,
        n_accounts: 2,
        n_txs: 400,
        workers: 6,
        speculation: true,
        eager: true,
        theta: 1000.0,
        max_inc: 3,
        max_delay_us: 100,
        batch: 16,
        ..Case::default()
    });
    // The case must actually speculate, fail validation and re-execute.
    assert!(summary.spec_dispatches > 0, "{summary:?}");
    assert!(summary.validation_failures + summary.eager_reexecs > 0, "{summary:?}");
    run_case(&Case {
        seed: 8,
        n_accounts: 2,
        n_txs: 400,
        workers: 6,
        speculation: true,
        eager: false,
        theta: 1000.0,
        max_inc: 1,
        max_delay_us: 100,
        batch: 400,
        ..Case::default()
    });
}

#[test]
fn test_hints_ewma() {
    let mut hints = Hints::new(0.5);
    let key = Pubkey::new_unique();
    assert_eq!(hints.get(&key), 0.5);
    hints.observe(&key, false);
    assert_eq!(hints.get(&key), 0.25);
    hints.observe(&key, true);
    assert_eq!(hints.get(&key), 0.625);
}

/// With the fast lane off (the default in tests), the coordinator's tick aborts every run
/// (releasing it) and forgets the hints.
#[test]
fn test_tick_releases_runs_when_disabled() {
    assert!(!crate::control::is_active());
    let key = Pubkey::new_unique();
    let run = Arc::new(MockRun {
        overlay: Overlay::new(2, Arc::new(MapBase(HashMap::new()))),
        txs: vec![
            MockTx {
                locks: vec![(key, true)],
                sinks: Arc::new(Vec::new()),
            };
            3
        ],
        max_delay_us: 0,
        seed: 1,
    });
    let (task_tx, _task_rx) = crossbeam_channel::unbounded();
    let sink = Arc::new(StdMutex::new(CollectSink::default()));
    let tunables = Arc::new(Tunables::new(true, true, 0.2, 3));
    let mut coord = Coordinator::new(2, task_tx, tunables, 0.25, Arc::clone(&sink));
    coord.handle(CoordMsg::NewRun { run_id: 9, run: run.clone() });
    coord.handle(CoordMsg::Txs {
        run_id: 9,
        first: 0,
        metas: (0..3)
            .map(|_| TxMeta {
                locks: vec![(key, true)],
                certain_writes: vec![key],
                is_vote: false,
                external: false,
            })
            .collect(),
        t_ingest: Instant::now(),
    });
    coord.hints.observe(&key, true);
    assert!(coord.run_state_counts(9).is_some());
    coord.tick();
    assert!(coord.run_state_counts(9).is_none(), "run released");
    assert_eq!(coord.hints.len(), 0);
    let sink = sink.lock().unwrap();
    assert_eq!(sink.ended.len(), 1);
    assert_eq!(sink.ended[0].1.aborted, Some("disabled"));
    drop(coord);
    assert_eq!(Arc::strong_count(&run), 1, "the coordinator dropped the run");
}

/// Delta rebase on: exact serial results for random DAGs (value-dependent and commutative
/// semantics, shared fee payers, every worker count / delay / threshold), and no leftover
/// predicted version (the final state check).
#[test]
fn test_serial_equivalence_rebase() {
    let mut rng = StdRng::seed_from_u64(4242);
    let mut totals = RunSummary::default();
    for i in 0..80u64 {
        let case = Case {
            seed: 5000 + i,
            n_accounts: rng.random_range(3..40),
            n_txs: rng.random_range(1..300),
            workers: [1usize, 2, 3, 6, 8][rng.random_range(0..5)],
            speculation: rng.random_bool(0.9),
            eager: rng.random_bool(0.9),
            theta: [0.0f32, 0.2, 0.5, 5.0, 1000.0][rng.random_range(0..5)],
            max_inc: rng.random_range(1..4),
            max_delay_us: [0u64, 50, 400][rng.random_range(0..3)],
            batch: rng.random_range(1..64),
            rebase: true,
            sinks: [0usize, 1, 2, 3][rng.random_range(0..4)],
            shared_payers: [0.0f64, 0.3, 0.8][rng.random_range(0..3)],
            feed_gap_us: 100,
            warm_hints: rng.random_bool(0.5),
        };
        let summary = run_case(&case);
        totals.predictions += summary.predictions;
        totals.shadow_predictions += summary.shadow_predictions;
        totals.pred_hits += summary.pred_hits;
        totals.pred_misses += summary.pred_misses;
        totals.spec_relaxed += summary.spec_relaxed;
        totals.final_fixups += summary.final_fixups;
    }
    println!("rebase totals: {totals:?}");
    // The mechanism must actually be exercised.
    assert!(totals.predictions > 0, "{totals:?}");
    assert!(totals.pred_hits > 0, "{totals:?}");
    assert!(totals.spec_relaxed > 0, "{totals:?}");
}

/// Payer chains and commutative sinks, blind-ish speculation, many workers: predictions
/// are made and verified, results stay serial.
#[test]
fn test_rebase_payer_chain_and_sinks() {
    for seed in 0..6u64 {
        let summary = run_case(&Case {
            seed: 900 + seed,
            n_accounts: 12,
            n_txs: 400,
            workers: 8,
            theta: 0.5,
            max_delay_us: 200,
            batch: 32,
            rebase: true,
            sinks: 3,
            shared_payers: 0.9,
            ..Case::default()
        });
        assert!(summary.predictions > 0, "{summary:?}");
        assert!(summary.pred_hits > 0, "{summary:?}");
        println!("payer/sink seed {seed}: {summary:?}");
    }
    // Same workload without rebase (control).
    let control = run_case(&Case {
        seed: 900,
        n_accounts: 12,
        n_txs: 400,
        workers: 8,
        theta: 0.5,
        max_delay_us: 200,
        batch: 32,
        rebase: false,
        sinks: 3,
        shared_payers: 0.9,
        ..Case::default()
    });
    println!("control: {control:?}");
}

#[test]
fn test_miss_hint_ewma() {
    let mut hints = Hints::new(0.5);
    let payer = Pubkey::new_unique();
    let other = Pubkey::new_unique();
    assert_eq!(hints.miss(&payer, true), MISS_PRIOR_PAYER);
    assert_eq!(hints.miss(&other, false), MISS_PRIOR);
    for _ in 0..40 {
        hints.observe_miss(&other, false, false);
    }
    assert!(hints.miss(&other, false) < 0.1);
    hints.observe_miss(&payer, true, true);
    assert!(hints.miss(&payer, true) > MISS_PRIOR_PAYER);
}

/// Latency comparison (not a correctness test): chains through shared fee payers and
/// commutative sinks, ~0.2 ms executions on 8 workers at moderate load. Run with
/// `cargo test --release -p agave-fast-lane --lib rebase_latency -- --ignored --nocapture`.
#[test]
#[ignore]
fn rebase_latency_bench() {
    for rebase in [false, true, false, true] {
        let mut all = Vec::new();
        let mut incs = 0u64;
        let mut n = 0u64;
        for seed in 0..4u64 {
            let summary = run_case(&Case {
                seed: 77 + seed,
                n_accounts: 400,
                n_txs: 600,
                workers: 8,
                theta: 0.5,
                max_delay_us: 800,
                batch: 12,
                feed_gap_us: 2000,
                rebase,
                sinks: 3,
                shared_payers: 0.5,
                warm_hints: true,
                ..Case::default()
            });
            incs += summary.incarnations;
            n += summary.finals;
            LAST_LATENCIES.with(|l| all.extend(l.borrow().iter().copied()));
        }
        all.sort_unstable();
        let p = |q: f64| all[((all.len() - 1) as f64 * q) as usize];
        println!(
            "rebase={rebase}: ingest->FINAL p50 {} p90 {} p99 {} us, incarnations/tx {:.2}",
            p(0.5),
            p(0.9),
            p(0.99),
            incs as f64 / n as f64
        );
    }
}

// ---------------------------------------------------------------------------------------
// Trace replay (not a unit test): real coordinator + worker threads over a production trace
// (per transaction: lock set, measured execution time, ingest time, and per written account
// whether the real transaction changed it additively — fee payer, fee sink — or in a
// value-dependent way). Time is dilated by FL_TRACE_DILATE (default 2) so sleeping workers
// are accurate; reported latencies are divided back. Also checks serial equivalence.
//   FL_TRACE=/path/trace.txt FL_TRACE_REBASE=0|1 cargo test -p agave-fast-lane --lib \
//     trace_replay -- --ignored --nocapture
// ---------------------------------------------------------------------------------------

struct TraceTx {
    k: TxIdx,
    vote: bool,
    err: bool,
    ing_us: u64,
    tap_us: u64,
    exec_us: u64,
    meas_us: i64,
    /// (key, writable, class, lamport delta, amount delta)
    locks: Vec<(Pubkey, bool, u8, i64, i64)>,
}

fn trace_key(id: u32) -> Pubkey {
    let mut b = [7u8; 32];
    b[..4].copy_from_slice(&id.to_le_bytes());
    Pubkey::new_from_array(b)
}

fn load_trace(path: &str) -> Vec<(Slot, Vec<TraceTx>)> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut slots: Vec<(Slot, Vec<TraceTx>)> = Vec::new();
    for line in text.lines() {
        let mut it = line.split(' ');
        let mut num = || it.next().unwrap().parse::<i64>().unwrap();
        let (slot, k, vote, err, ing, tap, exec, meas) =
            (num(), num(), num(), num(), num(), num(), num(), num());
        let locks = line
            .split(' ')
            .skip(8)
            .map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                (
                    trace_key(f[0].parse().unwrap()),
                    f[1] == "1",
                    f[2].as_bytes()[0],
                    f[3].parse().unwrap(),
                    f[4].parse().unwrap(),
                )
            })
            .collect();
        let tx = TraceTx {
            k: k as TxIdx,
            vote: vote == 1,
            err: err == 1,
            ing_us: ing as u64,
            tap_us: tap as u64,
            exec_us: exec as u64,
            meas_us: meas,
            locks,
        };
        match slots.last_mut() {
            Some((s, txs)) if *s == slot as Slot => {
                // Feed order is transaction order: ingest times non-decreasing.
                let mut tx = tx;
                tx.ing_us = tx.ing_us.max(txs.last().unwrap().ing_us);
                txs.push(tx)
            }
            _ => slots.push((slot as Slot, vec![tx])),
        }
    }
    slots
}

fn trace_semantics(
    k: TxIdx,
    tx: &TraceTx,
    reads: &[Option<AccountSharedData>],
) -> Vec<(Pubkey, AccountSharedData)> {
    let dep = fnv(
        tx.locks
            .iter()
            .zip(reads)
            .filter(|((_, _, cls, _, _), _)| *cls != b'p' && *cls != b's')
            .flat_map(|(_, r)| {
                r.as_ref()
                    .map(|a| {
                        let mut v = a.lamports().to_le_bytes().to_vec();
                        v.extend_from_slice(a.data());
                        v
                    })
                    .unwrap_or_else(|| vec![0xff])
            }),
        u64::from(k),
    );
    let mut writes = Vec::new();
    for ((key, w, cls, dl, da), r) in tx.locks.iter().zip(reads) {
        if !*w || (tx.err && *cls != b'p') {
            continue;
        }
        let mut a = r.clone().unwrap_or_else(|| {
            AccountSharedData::new(1_000_000_000_000, 8, &Pubkey::new_from_array([1; 32]))
        });
        match cls {
            b'p' | b's' => {
                a.set_lamports(a.lamports().saturating_add_signed(*dl).max(1));
                let word = u64::from_le_bytes(a.data()[..8].try_into().unwrap());
                a.data_as_mut_slice()[..8]
                    .copy_from_slice(&word.wrapping_add(*da as u64).to_le_bytes());
            }
            b'd' => {
                a.data_as_mut_slice()[..8]
                    .copy_from_slice(&fnv(key.to_bytes(), dep).to_le_bytes());
            }
            _ => {}
        }
        writes.push((*key, a));
    }
    writes
}

struct TraceRun {
    overlay: Overlay,
    txs: Arc<Vec<TraceTx>>,
    dilate: u64,
}

impl SchedRun for TraceRun {
    fn overlay(&self) -> &Overlay {
        &self.overlay
    }
    fn execute(&self, k: TxIdx, _inc: u32) -> ExecOutput {
        let exec_start = Instant::now();
        let tx = &self.txs[k as usize];
        let mut reads = Vec::with_capacity(tx.locks.len());
        let mut values = Vec::with_capacity(tx.locks.len());
        for (key, ..) in &tx.locks {
            let vis = self.overlay.visible_below(key, k);
            values.push(vis.value.clone());
            reads.push(Read {
                key: *key,
                origin: vis.origin,
                value: vis.value,
            });
        }
        let writes = trace_semantics(k, tx, &values);
        if self.dilate == 0 {
            // Discrete-event replay: the caller accounts execution time.
            return ExecOutput {
                reads,
                payload: Box::new(writes.clone()),
                writes,
                unprocessable: false,
                exec_start,
                exec_end: exec_start,
            };
        }
        let until = exec_start + Duration::from_micros(tx.exec_us * self.dilate);
        let now = Instant::now();
        if until > now + Duration::from_micros(300) {
            thread::sleep(until - now - Duration::from_micros(300));
        }
        while Instant::now() < until {
            std::hint::spin_loop();
        }
        ExecOutput {
            reads,
            payload: Box::new(writes.clone()),
            writes,
            unprocessable: false,
            exec_start,
            exec_end: Instant::now(),
        }
    }
}

#[derive(Default)]
struct TraceSink {
    finals: HashMap<(RunId, TxIdx), (Instant, u32, Vec<(Pubkey, AccountSharedData)>)>,
    ended: Vec<(RunId, RunSummary)>,
}

impl FinalSink for Arc<StdMutex<TraceSink>> {
    fn on_final(&mut self, f: Finalized) {
        let writes = *f
            .payload
            .downcast::<Vec<(Pubkey, AccountSharedData)>>()
            .unwrap();
        self.lock()
            .unwrap()
            .finals
            .insert((f.run_id, f.k), (f.t_final, f.incarnations, writes));
    }
    fn on_run_end(&mut self, run_id: RunId, summary: RunSummary) {
        self.lock().unwrap().ended.push((run_id, summary));
    }
}

#[test]
#[ignore]
fn trace_replay() {
    let Ok(path) = std::env::var("FL_TRACE") else {
        return;
    };
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let rebase = env("FL_TRACE_REBASE", 1) == 1;
    let dilate = env("FL_TRACE_DILATE", 2);
    let workers = env("FL_TRACE_WORKERS", 8) as usize;
    let theta = env("FL_TRACE_THETA_X100", 50) as f32 / 100.0;
    let max_slots = env("FL_TRACE_SLOTS", 1000) as usize;
    let slots: Vec<(Slot, Arc<Vec<TraceTx>>)> = load_trace(&path)
        .into_iter()
        .take(max_slots)
        .map(|(s, txs)| (s, Arc::new(txs)))
        .collect();
    let base = Arc::new(MapBase(HashMap::new()));

    let (task_tx, task_rx) = crossbeam_channel::unbounded();
    let (coord_tx, coord_rx) = crossbeam_channel::unbounded();
    let exit = Arc::new(AtomicBool::new(false));
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let task_rx = task_rx.clone();
            let coord_tx = coord_tx.clone();
            let exit = exit.clone();
            thread::spawn(move || worker_loop(task_rx, coord_tx, exit, Duration::from_micros(30)))
        })
        .collect();
    let sink = Arc::new(StdMutex::new(TraceSink::default()));
    let tunables = Arc::new(Tunables::new(true, true, theta, 3).with_rebase(rebase));
    let mut coord = Coordinator::new(workers, task_tx, tunables, 1.0 / 32.0, Arc::clone(&sink));

    // Feed on the trace's clock (dilated): one Txs message per distinct ingest time.
    let t0_us = slots.iter().map(|(_, txs)| txs[0].ing_us).min().unwrap();
    let start = Instant::now() + Duration::from_millis(50);
    let feeder = {
        let coord_tx = coord_tx.clone();
        let slots = slots.clone();
        let base = Arc::clone(&base);
        thread::spawn(move || {
            let mut events: Vec<(u64, usize, usize, usize)> = Vec::new(); // (t, slot idx, first, end)
            for (si, (_, txs)) in slots.iter().enumerate() {
                let mut first = 0;
                while first < txs.len() {
                    let t = txs[first].ing_us;
                    let mut end = first + 1;
                    while end < txs.len() && txs[end].ing_us == t {
                        end += 1;
                    }
                    events.push((t, si, first, end));
                    first = end;
                }
            }
            events.sort_by_key(|e| (e.0, e.1, e.2));
            let mut started = vec![false; slots.len()];
            let mut fed = vec![0usize; slots.len()];
            for (t, si, first, end) in events {
                let at = start + Duration::from_micros((t - t0_us) * dilate);
                while Instant::now() < at {
                    let now = Instant::now();
                    if at > now + Duration::from_micros(200) {
                        thread::sleep(at - now - Duration::from_micros(200));
                    } else {
                        std::hint::spin_loop();
                    }
                }
                let run_id = si as RunId + 1;
                if !started[si] {
                    started[si] = true;
                    let run = Arc::new(TraceRun {
                        overlay: Overlay::new(slots[si].0, base.clone() as Arc<dyn BaseReader>),
                        txs: Arc::clone(&slots[si].1),
                        dilate,
                    });
                    coord_tx.send(CoordMsg::NewRun { run_id, run }).unwrap();
                }
                // Transactions are fed in order: a later event of the same slot may carry an
                // earlier ingest time only if the trace is unordered; feed what is next.
                let (first, end) = (first.max(fed[si]), end.max(fed[si]));
                if first != fed[si] {
                    continue;
                }
                let txs = &slots[si].1;
                let metas = txs[first..end]
                    .iter()
                    .map(|tx| TxMeta {
                        locks: tx.locks.iter().map(|(k, w, ..)| (*k, *w)).collect(),
                        certain_writes: vec![tx.locks[0].0],
                        is_vote: tx.vote,
                        external: false,
                    })
                    .collect();
                coord_tx
                    .send(CoordMsg::Txs {
                        run_id,
                        first: first as TxIdx,
                        metas,
                        t_ingest: Instant::now(),
                    })
                    .unwrap();
                fed[si] = end;
                if end == txs.len() {
                    coord_tx
                        .send(CoordMsg::InputComplete {
                            run_id,
                            total: txs.len() as TxIdx,
                        })
                        .unwrap();
                }
            }
        })
    };
    let n_slots = slots.len();
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        if let Ok(msg) = coord_rx.recv_timeout(Duration::from_micros(200)) {
            coord.handle(msg);
            while let Ok(msg) = coord_rx.try_recv() {
                coord.handle(msg);
            }
            coord.dispatch();
        }
        if sink.lock().unwrap().ended.len() == n_slots {
            break;
        }
        assert!(Instant::now() < deadline, "stuck");
    }
    feeder.join().unwrap();
    exit.store(true, Ordering::Relaxed);
    drop(coord);
    drop(coord_tx);
    for h in handles {
        h.join().unwrap();
    }

    // Exactness vs serial, and latency of the measured (token, ring) transactions.
    let sink = sink.lock().unwrap();
    let mut lat = Vec::new();
    let mut meas = Vec::new();
    let mut incs = 0u64;
    let mut n = 0u64;
    for (si, (_, txs)) in slots.iter().enumerate() {
        let run_id = si as RunId + 1;
        let mut state: HashMap<Pubkey, AccountSharedData> = HashMap::new();
        for tx in txs.iter() {
            let values: Vec<_> = tx
                .locks
                .iter()
                .map(|(key, ..)| state.get(key).filter(|a| a.lamports() != 0).cloned())
                .collect();
            let writes = trace_semantics(tx.k, tx, &values);
            let (t_final, inc, got) = &sink.finals[&(run_id, tx.k)];
            assert_eq!(got.len(), writes.len(), "slot {si} tx {} write count", tx.k);
            for ((gk, ga), (ek, ea)) in got.iter().zip(&writes) {
                assert_eq!(gk, ek);
                assert!(crate::mv::accounts_equal(ga, ea), "slot {si} tx {} value", tx.k);
            }
            for (key, a) in writes {
                state.insert(key, a);
            }
            incs += u64::from(*inc);
            n += 1;
            if tx.meas_us >= 0 {
                let feed_at = start + Duration::from_micros((tx.ing_us - t0_us) * dilate);
                let fl = t_final.saturating_duration_since(feed_at).as_micros() as u64 / dilate
                    + (tx.ing_us - tx.tap_us);
                lat.push(fl);
                meas.push(tx.meas_us as u64);
            }
        }
    }
    let mut totals = RunSummary::default();
    for (_, s) in &sink.ended {
        totals.predictions += s.predictions;
        totals.shadow_predictions += s.shadow_predictions;
        totals.pred_hits += s.pred_hits;
        totals.pred_misses += s.pred_misses;
        totals.spec_relaxed += s.spec_relaxed;
        totals.final_fixups += s.final_fixups;
        totals.spec_dispatches += s.spec_dispatches;
        totals.validation_failures += s.validation_failures;
        totals.eager_reexecs += s.eager_reexecs;
    }
    let pct = |v: &mut Vec<u64>, q: f64| {
        v.sort_unstable();
        v[((v.len() - 1) as f64 * q) as usize]
    };
    println!(
        "TRACE rebase={rebase} K={workers} theta={theta} dilate={dilate} slots={n_slots}: \
         sim p50/p90/p99 {}/{}/{} us (measured {}/{}/{}), <=1.5ms {:.3} <=5ms {:.3}, \
         incarnations/tx {:.3}, {totals:?}",
        pct(&mut lat, 0.5),
        pct(&mut lat, 0.9),
        pct(&mut lat, 0.99),
        pct(&mut meas, 0.5),
        pct(&mut meas, 0.9),
        pct(&mut meas, 0.99),
        lat.iter().filter(|&&x| x <= 1500).count() as f64 / lat.len() as f64,
        lat.iter().filter(|&&x| x <= 5000).count() as f64 / lat.len() as f64,
        incs as f64 / n as f64,
    );
}

/// Virtual-time sink for the discrete-event replay.
struct DesSink {
    clock: Arc<std::sync::atomic::AtomicU64>,
    finals: HashMap<(RunId, TxIdx), (u64, u32, Vec<(Pubkey, AccountSharedData)>)>,
    ended: Vec<(RunId, RunSummary)>,
}

impl FinalSink for DesSink {
    fn on_final(&mut self, f: Finalized) {
        let writes = *f
            .payload
            .downcast::<Vec<(Pubkey, AccountSharedData)>>()
            .unwrap();
        let now = self.clock.load(Ordering::Relaxed);
        self.finals.insert((f.run_id, f.k), (now, f.incarnations, writes));
    }
    fn on_run_end(&mut self, run_id: RunId, summary: RunSummary) {
        self.ended.push((run_id, summary));
    }
}

/// Discrete-event replay of a production trace through the real coordinator: K virtual
/// workers, each incarnation reads the overlay when it starts and installs its writes after
/// the transaction's measured execution time; the coordinator's own latency is a fixed
/// `FL_DES_COORD_US` per completion (default 12 µs) and dispatch-to-start `FL_DES_START_US`
/// (default 2 µs). Deterministic, no OS noise. The trace is replayed `FL_DES_PASSES` times
/// (default 2) on one coordinator (hints warm up); the last pass is measured. Also checks
/// serial equivalence of every pass.
///   FL_TRACE=/path/trace.txt FL_TRACE_REBASE=0|1 cargo test --release -p agave-fast-lane \
///     --lib trace_des -- --ignored --nocapture
#[test]
#[ignore]
fn trace_des() {
    let Ok(path) = std::env::var("FL_TRACE") else {
        return;
    };
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let rebase = env("FL_TRACE_REBASE", 1) == 1;
    let workers = env("FL_TRACE_WORKERS", 8) as usize;
    let theta = env("FL_TRACE_THETA_X100", 50) as f32 / 100.0;
    let max_slots = env("FL_TRACE_SLOTS", 1000) as usize;
    let passes = env("FL_DES_PASSES", 2) as usize;
    let coord_us = env("FL_DES_COORD_US", 12);
    let exec_scale = env("FL_DES_EXEC_PCT", 100);
    let start_us = env("FL_DES_START_US", 2);
    // Upper bound: payer and sink writes do not lock (never create an edge).
    let drop_soft = env("FL_DES_DROP_SOFT", 0) == 1;
    let trace: Vec<(Slot, Arc<Vec<TraceTx>>)> = load_trace(&path)
        .into_iter()
        .take(max_slots)
        .map(|(s, mut txs)| {
            if drop_soft {
                for tx in &mut txs {
                    tx.locks.retain(|(_, w, cls, ..)| !(*w && (*cls == b'p' || *cls == b's')));
                }
            }
            (s, Arc::new(txs))
        })
        .collect();
    let t0 = trace.iter().map(|(_, txs)| txs[0].ing_us).min().unwrap();
    let t_end = trace
        .iter()
        .map(|(_, txs)| txs.last().unwrap().ing_us)
        .max()
        .unwrap();
    let pass_len = t_end - t0 + 1_000_000;
    let base = Arc::new(MapBase(HashMap::new()));

    let (task_tx, task_rx) = crossbeam_channel::unbounded::<WorkerTask>();
    let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let max_inc = env("FL_DES_MAX_INC", 3) as u32;
    let tunables = Arc::new(Tunables::new(true, true, theta, max_inc).with_rebase(rebase));
    let mut coord = Coordinator::new(
        workers,
        task_tx,
        tunables,
        1.0 / 32.0,
        DesSink {
            clock: Arc::clone(&clock),
            finals: HashMap::new(),
            ended: Vec::new(),
        },
    );

    let envf = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    coord.hints.miss_prior_payer = envf("FL_DES_PRIOR_PAYER", coord.hints.miss_prior_payer);
    coord.hints.miss_prior = envf("FL_DES_PRIOR", coord.hints.miss_prior);
    coord.hints.certain_miss_max = envf("FL_DES_CERTAIN_MAX", coord.hints.certain_miss_max);
    coord.hints.predict_miss_max = envf("FL_DES_PREDICT_MAX", coord.hints.predict_miss_max);
    coord.hints.miss_alpha = envf("FL_DES_MISS_ALPHA", coord.hints.miss_alpha);
    // Events: (time, seq, kind). kind 0 = feed (pass, slot idx, first, end); 1 = done.
    enum Ev {
        Feed(usize, usize, usize, usize),
        Done(WorkerTask, ExecOutput),
    }
    let mut events: BinaryHeap<Reverse<(u64, u64)>> = BinaryHeap::new();
    let mut payloads: HashMap<u64, Ev> = HashMap::new();
    let mut seq = 0u64;
    let mut push = |events: &mut BinaryHeap<Reverse<(u64, u64)>>,
                    payloads: &mut HashMap<u64, Ev>,
                    t: u64,
                    ev: Ev| {
        seq += 1;
        events.push(Reverse((t, seq)));
        payloads.insert(seq, ev);
    };
    for pass in 0..passes {
        for (si, (_, txs)) in trace.iter().enumerate() {
            let mut first = 0;
            while first < txs.len() {
                let t = txs[first].ing_us;
                let mut end = first + 1;
                while end < txs.len() && txs[end].ing_us == t {
                    end += 1;
                }
                push(
                    &mut events,
                    &mut payloads,
                    (t - t0) + pass as u64 * pass_len,
                    Ev::Feed(pass, si, first, end),
                );
                first = end;
            }
        }
    }
    let run_id_of = |pass: usize, si: usize| (pass * trace.len() + si) as RunId + 1;
    let mut started = vec![false; passes * trace.len()];
    let mut runs: HashMap<RunId, Arc<TraceRun>> = HashMap::new();
    let mut busy = 0usize;
    while let Some(Reverse((t, id))) = events.pop() {
        clock.store(t, Ordering::Relaxed);
        match payloads.remove(&id).unwrap() {
            Ev::Feed(pass, si, first, end) => {
                let run_id = run_id_of(pass, si);
                if !started[run_id as usize - 1] {
                    started[run_id as usize - 1] = true;
                    let run = Arc::new(TraceRun {
                        overlay: Overlay::new(trace[si].0, base.clone() as Arc<dyn BaseReader>),
                        txs: Arc::clone(&trace[si].1),
                        dilate: 0,
                    });
                    runs.insert(run_id, Arc::clone(&run));
                    coord.handle(CoordMsg::NewRun { run_id, run });
                }
                let txs = &trace[si].1;
                let metas = txs[first..end]
                    .iter()
                    .map(|tx| TxMeta {
                        locks: tx.locks.iter().map(|(k, w, ..)| (*k, *w)).collect(),
                        certain_writes: if drop_soft || tx.locks.is_empty() {
                            Vec::new()
                        } else {
                            vec![tx.locks[0].0]
                        },
                        is_vote: tx.vote,
                        external: false,
                    })
                    .collect();
                coord.handle(CoordMsg::Txs {
                    run_id,
                    first: first as TxIdx,
                    metas,
                    t_ingest: Instant::now(),
                });
                if end == txs.len() {
                    coord.handle(CoordMsg::InputComplete {
                        run_id,
                        total: txs.len() as TxIdx,
                    });
                }
            }
            Ev::Done(task, out) => {
                busy -= 1;
                task.run
                    .overlay()
                    .install(task.k, task.inc, &out.writes, &task.prev_writes);
                coord.handle(CoordMsg::Done {
                    run_id: task.run_id,
                    k: task.k,
                    inc: task.inc,
                    out,
                });
            }
        }
        coord.dispatch();
        while let Ok(task) = task_rx.try_recv() {
            busy += 1;
            assert!(busy <= workers);
            let si = (task.run_id as usize - 1) % trace.len();
            let exec_us = trace[si].1[task.k as usize].exec_us * exec_scale / 100;
            let out = task.run.execute(task.k, task.inc);
            push(
                &mut events,
                &mut payloads,
                t + start_us + exec_us + coord_us,
                Ev::Done(task, out),
            );
        }
    }
    let sink = coord.sink_mut();
    assert_eq!(sink.ended.len(), passes * trace.len(), "every run ended");
    let mut lat = Vec::new();
    let mut meas = Vec::new();
    let mut incs = 0u64;
    let mut n = 0u64;
    for pass in 0..passes {
        for (si, (_, txs)) in trace.iter().enumerate() {
            let run_id = run_id_of(pass, si);
            let mut state: HashMap<Pubkey, AccountSharedData> = HashMap::new();
            for tx in txs.iter() {
                let values: Vec<_> = tx
                    .locks
                    .iter()
                    .map(|(key, ..)| state.get(key).filter(|a| a.lamports() != 0).cloned())
                    .collect();
                let writes = trace_semantics(tx.k, tx, &values);
                let (t_final, inc, got) = &sink.finals[&(run_id, tx.k)];
                assert_eq!(got.len(), writes.len(), "pass {pass} slot {si} tx {}", tx.k);
                for ((gk, ga), (ek, ea)) in got.iter().zip(&writes) {
                    assert_eq!(gk, ek);
                    assert!(
                        drop_soft || crate::mv::accounts_equal(ga, ea),
                        "pass {pass} slot {si} tx {}",
                        tx.k
                    );
                }
                for (key, a) in writes {
                    state.insert(key, a);
                }
                if pass + 1 == passes {
                    incs += u64::from(*inc);
                    n += 1;
                    if tx.meas_us >= 0 {
                        let ing_v = (tx.ing_us - t0) + pass as u64 * pass_len;
                        lat.push(t_final - ing_v + (tx.ing_us - tx.tap_us));
                        meas.push(tx.meas_us as u64);
                    }
                }
            }
        }
    }
    let mut totals = RunSummary::default();
    for (run_id, s) in &sink.ended {
        if (*run_id as usize - 1) / trace.len() + 1 != passes {
            continue;
        }
        totals.predictions += s.predictions;
        totals.shadow_predictions += s.shadow_predictions;
        totals.pred_hits += s.pred_hits;
        totals.pred_misses += s.pred_misses;
        totals.spec_relaxed += s.spec_relaxed;
        totals.final_fixups += s.final_fixups;
        totals.spec_dispatches += s.spec_dispatches;
        totals.nonspec_dispatches += s.nonspec_dispatches;
        totals.validation_failures += s.validation_failures;
        totals.eager_reexecs += s.eager_reexecs;
    }
    let pct = |v: &mut Vec<u64>, q: f64| {
        v.sort_unstable();
        v[((v.len() - 1) as f64 * q) as usize]
    };
    println!(
        "DES rebase={rebase} K={workers} theta={theta} slots={} passes={passes}: \
         p50/p90/p99 {}/{}/{} us (measured {}/{}/{}), <=1.5ms {:.3} <=5ms {:.3}, \
         incarnations/tx {:.3}, {totals:?}",
        trace.len(),
        pct(&mut lat, 0.5),
        pct(&mut lat, 0.9),
        pct(&mut lat, 0.99),
        pct(&mut meas, 0.5),
        pct(&mut meas, 0.9),
        pct(&mut meas, 0.99),
        lat.iter().filter(|&&x| x <= 1500).count() as f64 / lat.len() as f64,
        lat.iter().filter(|&&x| x <= 5000).count() as f64 / lat.len() as f64,
        incs as f64 / n as f64,
    );
}
