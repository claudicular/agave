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
}

/// Deterministic semantics of transaction `k` given the values it read, in lock order.
fn mock_semantics(
    k: TxIdx,
    tx: &MockTx,
    reads: &[Option<AccountSharedData>],
) -> Vec<(Pubkey, AccountSharedData)> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&k.to_le_bytes());
    for r in reads {
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
}

impl FinalSink for Arc<StdMutex<CollectSink>> {
    fn on_final(&mut self, f: Finalized) {
        let writes = *f
            .payload
            .downcast::<Vec<(Pubkey, AccountSharedData)>>()
            .unwrap();
        self.lock().unwrap().finals.push((f.k, f.incarnations, writes));
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
    let mut txs = Vec::new();
    for _ in 0..case.n_txs {
        let n = rng.random_range(1..=5);
        let mut locks: Vec<(Pubkey, bool)> = Vec::new();
        // Fee payers: mostly distinct, sometimes shared (payer chains).
        let payer = if rng.random_bool(0.3) {
            keys[rng.random_range(0..keys.len())]
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
            locks.push((key, rng.random_bool(0.6)));
        }
        txs.push(MockTx { locks });
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
    let tunables = Arc::new(Tunables::new(case.speculation, case.eager, case.theta, case.max_inc));
    let mut coord = Coordinator::new(case.workers, task_tx, tunables, 0.25, Arc::clone(&sink));
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
                thread::sleep(Duration::from_micros(100));
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
    let (_, summary) = &sink.ended[0];
    assert_eq!(summary.finals as usize, case.n_txs);
    assert!(summary.aborted.is_none());
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
