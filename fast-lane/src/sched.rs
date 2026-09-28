//! Lock-aware Block-STM ("LA-BSTM") coordinator and executor pool.
//!
//! Solana transactions declare their write-lock sets, so every potential writer of every
//! account is known at ingest. `preds(k)` is the last write-locker before `k` of each
//! account `k` locks (plus, after execution, of each account it read outside its lock set).
//! Once all of `preds(k)` are FINAL, every earlier writer of anything `k` read is FINAL
//! (each pred waited for its own preds), so `k` can be validated exactly: every recorded
//! read must equal the final value below `k`. A valid `k` is FINAL; an invalid one is
//! re-executed on final inputs (then valid by construction).
//!
//! Execution may start before the preds are final ("speculation") when every unexecuted
//! earlier write-locker of every locked account is unlikely to change it (per-account
//! change-probability hint) and is not a certain writer of it (fee payer, nonce, vote
//! account). A hint only chooses what runs in parallel; it never affects results.
//!
//! Records are emitted at FINAL, so the records of any one account appear in block order.
//!
//! Delta rebase (`rebase` tunable, DESIGN §19): when an executed, not-yet-final
//! transaction's input changes, the coordinator installs a *predicted* version of its
//! outputs (its own change re-applied to the new input, [`crate::mv::rebase`]) so later
//! transactions speculate on it instead of waiting for the re-execution, and accounts whose
//! predictions verify (fee payers, fee sinks) no longer block speculation. Predictions are
//! only speculative inputs: they are never final, never emitted, and replaced by the
//! executed value before a transaction is FINAL; every FINAL read is still checked against
//! executed final values, so a wrong prediction costs a re-execution, never exactness.

use {
    crate::{
        control::Tunables,
        mv::{Origin, Overlay, PRED_INC_BIT, Read, TxIdx, accounts_equal, rebase, same_value},
    },
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError},
    log::warn,
    solana_account::AccountSharedData,
    solana_pubkey::Pubkey,
    std::{
        any::Any,
        cmp::Reverse,
        collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    },
};

type FastMap<K, V> = HashMap<K, V, ahash::RandomState>;

/// A run the scheduler can execute: an overlay plus a way to execute one incarnation.
pub trait SchedRun: Send + Sync + 'static {
    fn overlay(&self) -> &Overlay;
    /// Execute incarnation `inc` of transaction `k`, reading through the overlay. Must not
    /// install anything; the executor installs `writes` afterwards.
    fn execute(&self, k: TxIdx, inc: u32) -> ExecOutput;
    /// Every transaction of the run is FINAL (called once, not for aborted runs).
    fn on_complete(&self, _summary: &RunSummary) {}
}

/// Result of one incarnation.
pub struct ExecOutput {
    pub reads: Vec<Read>,
    /// The accounts this incarnation writes (agave's store filter), installed as versions.
    pub writes: Vec<(Pubkey, AccountSharedData)>,
    /// Opaque per-transaction outcome, handed to the sink at FINAL.
    pub payload: Box<dyn Any + Send>,
    /// Transaction is unprocessable: agave will mark the slot dead.
    pub unprocessable: bool,
    pub exec_start: Instant,
    pub exec_end: Instant,
}

/// Static per-transaction scheduling input, produced at ingest.
#[derive(Debug, Clone)]
pub struct TxMeta {
    /// Every account key with its write flag, as agave locks it.
    pub locks: Vec<(Pubkey, bool)>,
    /// Accounts this transaction always writes (fee payer, nonce, vote account).
    pub certain_writes: Vec<Pubkey>,
    pub is_vote: bool,
    /// Never dispatched: completed from outside with [`CoordMsg::ExternalDone`] (a chained
    /// run's "parent freeze" pseudo-transaction).
    pub external: bool,
}

pub type RunId = u64;

pub enum CoordMsg {
    NewRun {
        run_id: RunId,
        run: Arc<dyn SchedRun>,
    },
    /// Transactions `first..first + metas.len()` of the run, in order.
    Txs {
        run_id: RunId,
        first: TxIdx,
        metas: Vec<TxMeta>,
        t_ingest: Instant,
    },
    /// No more transactions will be added; the run has `total` transactions.
    InputComplete {
        run_id: RunId,
        total: TxIdx,
    },
    AbortRun {
        run_id: RunId,
        reason: &'static str,
    },
    Done {
        run_id: RunId,
        k: TxIdx,
        inc: u32,
        out: ExecOutput,
    },
    /// Completion of an external transaction; its versions are already installed.
    ExternalDone {
        run_id: RunId,
        k: TxIdx,
        out: ExecOutput,
    },
    Shutdown,
}

/// A transaction that became FINAL.
pub struct Finalized {
    pub run_id: RunId,
    pub k: TxIdx,
    pub incarnations: u32,
    /// The validated incarnation ran while some predecessor was not final.
    pub speculative: bool,
    pub payload: Box<dyn Any + Send>,
    pub t_ingest: Instant,
    /// First dispatch of any incarnation to a worker.
    pub t_first_dispatch: Instant,
    /// Every predecessor FINAL (ingest time if none was pending).
    pub t_ready: Instant,
    /// Predicted versions the coordinator installed for this transaction.
    pub rebased: u32,
    pub t_exec_start: Instant,
    pub t_exec_end: Instant,
    pub t_final: Instant,
    pub n_preds: usize,
}

#[derive(Debug, Default, Clone)]
pub struct RunSummary {
    pub txs: u64,
    pub finals: u64,
    pub incarnations: u64,
    pub spec_dispatches: u64,
    pub nonspec_dispatches: u64,
    pub validation_failures: u64,
    pub eager_reexecs: u64,
    pub aborted: Option<&'static str>,
    /// FINAL transactions whose validated outcome is unprocessable.
    pub unprocessable: u64,
    /// Predicted (delta-rebased) versions installed.
    pub predictions: u64,
    /// Predictions checked by the transaction's next incarnation on the same input: equal
    /// (hit) or not (miss).
    pub pred_hits: u64,
    pub pred_misses: u64,
    /// Transactions made speculative only by the rebase-aware rule.
    pub spec_relaxed: u64,
    /// Versions replaced/inserted/removed at FINAL so that the final version is the
    /// executed result (a prediction was still installed).
    pub final_fixups: u64,
}

/// A prediction the coordinator installed for a transaction, checked when its next
/// incarnation completes (hint input only).
struct PredRec {
    key: Pubkey,
    /// The input value the prediction was computed for.
    basis: AccountSharedData,
    predicted: AccountSharedData,
}

impl PredRec {
    fn bytes(&self) -> i64 {
        2 * crate::mem::ACCOUNT_OVERHEAD + solana_account::ReadableAccount::data(&self.predicted).len() as i64
    }
}

/// Fee payers get this rebase-miss prior (their change is usually the fee alone).
const MISS_PRIOR_PAYER: f32 = 0.25;
/// Other accounts: no speculation credit until predictions have verified.
const MISS_PRIOR: f32 = 1.0;
/// An unexecuted certain writer (fee payer, nonce) blocks speculation unless the account's
/// rebase-miss rate is at most this.
const CERTAIN_MISS_MAX: f32 = 0.3;
/// EWMA weight of the rebase-miss hint.
const MISS_ALPHA: f32 = 1.0 / 16.0;
/// Most readers one cascade step examines (bounds coordinator time per event).
const CASCADE_BUDGET: usize = 4096;

/// Receives FINAL transactions and run ends (runs on the coordinator thread).
pub trait FinalSink: Send {
    fn on_final(&mut self, finalized: Finalized);
    fn on_run_end(&mut self, run_id: RunId, summary: RunSummary);
    /// An abort for a run that already ended (e.g. agave marked the slot dead afterwards).
    fn on_abort_ended(&mut self, _run_id: RunId, _reason: &'static str) {}
    /// Called about every millisecond by the coordinator loop, busy or idle.
    fn tick(&mut self) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    Waiting,
    Ready,
    Running,
    Executed,
    Final,
}

struct TxS {
    locks: Vec<(u32, bool)>,
    is_vote: bool,
    external: bool,
    preds: Vec<TxIdx>,
    pending: u32,
    succs: Vec<TxIdx>,
    state: St,
    next_inc: u32,
    spec_incs: u32,
    running_spec: bool,
    out: Option<ExecOutput>,
    out_spec: bool,
    write_keys: Vec<Pubkey>,
    token: u32,
    executed_once: bool,
    t_ingest: Instant,
    t_first_dispatch: Option<Instant>,
    /// Incarnation that produced `out`.
    out_inc: u32,
    /// Predictions installed since `out` (verified by the next incarnation).
    pred_recs: Vec<PredRec>,
    rebased: u32,
    t_ready: Option<Instant>,
}

struct AcctS {
    key: Pubkey,
    lockers: Vec<(TxIdx, bool)>,
    last_writer: Option<TxIdx>,
    unexec_w: BTreeSet<TxIdx>,
    unexec_certain: BTreeSet<TxIdx>,
    /// Fee payer of some transaction of the run.
    payer: bool,
}

struct RunS {
    run: Arc<dyn SchedRun>,
    order: u64,
    txs: Vec<TxS>,
    acct_idx: FastMap<Pubkey, u32>,
    accts: Vec<AcctS>,
    n_final: u32,
    total: Option<TxIdx>,
    running: u32,
    summary: RunSummary,
    /// Sequence for predicted incarnation numbers.
    pred_seq: u32,
    /// Bytes held by `PredRec`s of this run (in `mem::PRED_BYTES`).
    pred_bytes: i64,
}

impl Drop for RunS {
    fn drop(&mut self) {
        crate::mem::PRED_BYTES.sub(self.pred_bytes);
    }
}

/// Priority class: lower runs first.
const CLASS_RETRY: u8 = 0;
const CLASS_NONSPEC: u8 = 1;
const CLASS_SPEC: u8 = 2;
const CLASS_VOTE: u8 = 3;

type HeapKey = Reverse<(u8, u64, TxIdx, u32, RunId)>;

/// Global per-account hints: change probability (EWMA over FINAL write-lockers) and
/// rebase-miss rate (EWMA over verified predictions).
pub struct Hints {
    phat: FastMap<Pubkey, f32>,
    alpha: f32,
    prior: f32,
    cap: usize,
    miss: FastMap<Pubkey, f32>,
}

impl Hints {
    pub fn new(alpha: f32) -> Self {
        Self {
            phat: FastMap::default(),
            alpha,
            prior: 0.5,
            cap: 262_144,
            miss: FastMap::default(),
        }
    }
    /// Probability that a change of `key` is not predicted by rebasing the writer's own
    /// change (learned; prior by kind).
    pub fn miss(&self, key: &Pubkey, payer: bool) -> f32 {
        self.miss.get(key).copied().unwrap_or(if payer {
            MISS_PRIOR_PAYER
        } else {
            MISS_PRIOR
        })
    }
    pub fn observe_miss(&mut self, key: &Pubkey, payer: bool, missed: bool) {
        if self.miss.len() >= self.cap && !self.miss.contains_key(key) {
            self.miss.clear();
        }
        let x = if missed { 1.0 } else { 0.0 };
        let prior = if payer { MISS_PRIOR_PAYER } else { MISS_PRIOR };
        let p = self.miss.entry(*key).or_insert(prior);
        *p += MISS_ALPHA * (x - *p);
    }
    pub fn miss_len(&self) -> usize {
        self.miss.len()
    }
    pub fn get(&self, key: &Pubkey) -> f32 {
        self.phat.get(key).copied().unwrap_or(self.prior)
    }
    pub fn observe(&mut self, key: &Pubkey, changed: bool) {
        if self.phat.len() >= self.cap && !self.phat.contains_key(key) {
            // Bounded: forget everything (hints re-warm within a slot on hot accounts).
            self.phat.clear();
        }
        let x = if changed { 1.0 } else { 0.0 };
        let p = self.phat.entry(*key).or_insert(self.prior);
        *p += self.alpha * (x - *p);
    }
    pub fn len(&self) -> usize {
        self.phat.len()
    }
    pub fn alpha(&self) -> f32 {
        self.alpha
    }
    pub fn is_empty(&self) -> bool {
        self.phat.is_empty() && self.miss.is_empty()
    }
}

pub struct WorkerTask {
    run_id: RunId,
    run: Arc<dyn SchedRun>,
    k: TxIdx,
    inc: u32,
    prev_writes: Vec<Pubkey>,
}

/// Executor thread body: execute, install versions, report.
pub fn worker_loop(
    tasks: Receiver<WorkerTask>,
    done: Sender<CoordMsg>,
    exit: Arc<AtomicBool>,
    spin: Duration,
) {
    loop {
        let task = match recv_spin(&tasks, spin, &exit) {
            Some(task) => task,
            None => return,
        };
        let out = task.run.execute(task.k, task.inc);
        task.run
            .overlay()
            .install(task.k, task.inc, &out.writes, &task.prev_writes);
        if done
            .send(CoordMsg::Done {
                run_id: task.run_id,
                k: task.k,
                inc: task.inc,
                out,
            })
            .is_err()
        {
            return;
        }
    }
}

/// Spin durations at or above this never park (for threads that own their core).
pub const SPIN_FOREVER: Duration = Duration::from_secs(1);

/// Receive with a bounded busy-poll before parking; `None` on exit/disconnect. A spin of
/// [`SPIN_FOREVER`] or more busy-polls without ever parking.
/// Sink tick period.
const TICK: Duration = Duration::from_millis(1);

pub fn recv_spin<T>(rx: &Receiver<T>, spin: Duration, exit: &AtomicBool) -> Option<T> {
    if spin >= SPIN_FOREVER {
        let mut polls = 0u32;
        loop {
            match rx.try_recv() {
                Ok(v) => return Some(v),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => {}
            }
            polls = polls.wrapping_add(1);
            if polls % 4096 == 0 && exit.load(Ordering::Relaxed) {
                return None;
            }
            std::hint::spin_loop();
        }
    }
    let start = Instant::now();
    loop {
        match rx.try_recv() {
            Ok(v) => return Some(v),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => {}
        }
        if start.elapsed() >= spin {
            break;
        }
        std::hint::spin_loop();
    }
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(v) => return Some(v),
            Err(RecvTimeoutError::Disconnected) => return None,
            Err(RecvTimeoutError::Timeout) => {
                if exit.load(Ordering::Relaxed) {
                    return None;
                }
            }
        }
    }
}

pub struct Coordinator<S: FinalSink> {
    runs: HashMap<RunId, RunS>,
    next_order: u64,
    heap: BinaryHeap<HeapKey>,
    running_total: usize,
    workers: usize,
    task_tx: Sender<WorkerTask>,
    tunables: Arc<Tunables>,
    pub hints: Hints,
    sink: S,
    worklist: Vec<(RunId, TxIdx)>,
}

impl<S: FinalSink> Coordinator<S> {
    pub fn new(
        workers: usize,
        task_tx: Sender<WorkerTask>,
        tunables: Arc<Tunables>,
        hint_alpha: f32,
        sink: S,
    ) -> Self {
        Self {
            runs: HashMap::new(),
            next_order: 0,
            heap: BinaryHeap::new(),
            running_total: 0,
            workers,
            task_tx,
            tunables,
            hints: Hints::new(hint_alpha),
            sink,
            worklist: Vec::new(),
        }
    }

    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    pub fn active_runs(&self) -> usize {
        self.runs.len()
    }

    /// Main loop: handle messages, dispatch, until shutdown or exit.
    pub fn run_loop(&mut self, rx: Receiver<CoordMsg>, exit: Arc<AtomicBool>, spin: Duration) {
        let mut last_tick = Instant::now();
        loop {
            let msg = if spin >= SPIN_FOREVER {
                // Busy-poll inline so the coordinator can tick while idle.
                let mut polls = 0u32;
                loop {
                    match rx.try_recv() {
                        Ok(msg) => break Some(msg),
                        Err(TryRecvError::Disconnected) => break None,
                        Err(TryRecvError::Empty) => {}
                    }
                    polls = polls.wrapping_add(1);
                    if polls % 1024 == 0 {
                        if exit.load(Ordering::Relaxed) {
                            break None;
                        }
                        if last_tick.elapsed() >= TICK {
                            self.tick();
                            last_tick = Instant::now();
                        }
                    }
                    std::hint::spin_loop();
                }
            } else {
                // Spin for `spin`, then block in short slices so the coordinator still ticks.
                let start = Instant::now();
                let mut got = None;
                let mut disconnected = false;
                while start.elapsed() < spin {
                    match rx.try_recv() {
                        Ok(msg) => {
                            got = Some(msg);
                            break;
                        }
                        Err(TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                        Err(TryRecvError::Empty) => std::hint::spin_loop(),
                    }
                }
                while got.is_none() && !disconnected {
                    match rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(msg) => got = Some(msg),
                        Err(RecvTimeoutError::Disconnected) => disconnected = true,
                        Err(RecvTimeoutError::Timeout) => {
                            if exit.load(Ordering::Relaxed) {
                                disconnected = true;
                            } else if last_tick.elapsed() >= TICK {
                                self.tick();
                                last_tick = Instant::now();
                            }
                        }
                    }
                }
                got
            };
            let Some(msg) = msg else {
                return;
            };
            if last_tick.elapsed() >= TICK {
                self.tick();
                last_tick = Instant::now();
            }
            if !self.handle(msg) {
                return;
            }
            // Drain whatever else is queued before dispatching.
            loop {
                match rx.try_recv() {
                    Ok(msg) => {
                        if !self.handle(msg) {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            self.dispatch();
        }
    }

    /// About every millisecond: the sink's tick; with the fast lane off, abort every run
    /// (releasing its memory) and forget the hints.
    pub fn tick(&mut self) {
        self.sink.tick();
        if !crate::control::is_active() {
            let run_ids: Vec<RunId> = self.runs.keys().copied().collect();
            for run_id in run_ids {
                self.handle(CoordMsg::AbortRun {
                    run_id,
                    reason: "disabled",
                });
            }
            if !self.hints.is_empty() {
                self.hints = Hints::new(self.hints.alpha());
            }
        }
        crate::mem::HINT_BYTES.set((self.hints.len() + self.hints.miss_len()) as i64 * 64);
    }

    /// Handle one message. Returns false on shutdown.
    pub fn handle(&mut self, msg: CoordMsg) -> bool {
        match msg {
            CoordMsg::NewRun { run_id, run } => {
                let order = self.next_order;
                self.next_order += 1;
                self.runs.insert(
                    run_id,
                    RunS {
                        run,
                        order,
                        txs: Vec::new(),
                        acct_idx: FastMap::default(),
                        accts: Vec::new(),
                        n_final: 0,
                        total: None,
                        running: 0,
                        summary: RunSummary::default(),
                        pred_seq: 0,
                        pred_bytes: 0,
                    },
                );
            }
            CoordMsg::Txs {
                run_id,
                first,
                metas,
                t_ingest,
            } => self.ingest_txs(run_id, first, metas, t_ingest),
            CoordMsg::InputComplete { run_id, total } => {
                if let Some(run) = self.runs.get_mut(&run_id) {
                    run.total = Some(total);
                }
                self.maybe_end_run(run_id);
            }
            CoordMsg::AbortRun { run_id, reason } => {
                if let Some(mut run) = self.runs.remove(&run_id) {
                    self.running_total -= run.running as usize;
                    run.summary.aborted = Some(reason);
                    run.summary.txs = run.txs.len() as u64;
                    run.summary.finals = u64::from(run.n_final);
                    self.sink.on_run_end(run_id, std::mem::take(&mut run.summary));
                } else {
                    self.sink.on_abort_ended(run_id, reason);
                }
            }
            CoordMsg::Done {
                run_id,
                k,
                inc,
                out,
            } => {
                let Some(run) = self.runs.get_mut(&run_id) else {
                    // Aborted run: its running count was already released.
                    return true;
                };
                run.running -= 1;
                self.running_total -= 1;
                self.on_done(run_id, k, inc, out);
                self.drain_worklist();
                self.maybe_end_run(run_id);
            }
            CoordMsg::ExternalDone { run_id, k, out } => {
                let Some(run) = self.runs.get_mut(&run_id) else {
                    return true;
                };
                let Some(tx) = run.txs.get_mut(k as usize) else {
                    return true;
                };
                if !tx.external || tx.state == St::Final || tx.next_inc > 0 {
                    return true;
                }
                tx.state = St::Running;
                tx.t_first_dispatch.get_or_insert_with(Instant::now);
                tx.next_inc = 1;
                run.summary.incarnations += 1;
                self.on_done(run_id, k, 0, out);
                self.drain_worklist();
                self.maybe_end_run(run_id);
            }
            CoordMsg::Shutdown => return false,
        }
        true
    }

    fn acct_index(run: &mut RunS, key: &Pubkey) -> u32 {
        if let Some(&i) = run.acct_idx.get(key) {
            return i;
        }
        let i = run.accts.len() as u32;
        run.accts.push(AcctS {
            key: *key,
            lockers: Vec::new(),
            last_writer: None,
            unexec_w: BTreeSet::new(),
            unexec_certain: BTreeSet::new(),
            payer: false,
        });
        run.acct_idx.insert(*key, i);
        i
    }

    fn ingest_txs(&mut self, run_id: RunId, first: TxIdx, metas: Vec<TxMeta>, t_ingest: Instant) {
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        if first as usize != run.txs.len() {
            warn!(
                "fast lane: run {run_id} got txs at {first}, expected {}",
                run.txs.len()
            );
            return;
        }
        let mut new_ready = Vec::new();
        for (i, meta) in metas.into_iter().enumerate() {
            let k = first + i as TxIdx;
            let locks: Vec<(u32, bool)> = meta
                .locks
                .iter()
                .map(|(key, w)| (Self::acct_index(run, key), *w))
                .collect();
            let certain: Vec<u32> = meta
                .certain_writes
                .iter()
                .map(|key| Self::acct_index(run, key))
                .collect();
            // `certain_writes[0]` is the fee payer (`Run::push_entries`).
            if let Some(&payer) = certain.first() {
                if !meta.external {
                    run.accts[payer as usize].payer = true;
                }
            }
            let mut preds: Vec<TxIdx> = Vec::with_capacity(locks.len());
            for &(a, _) in &locks {
                if let Some(pw) = run.accts[a as usize].last_writer {
                    if !preds.contains(&pw) {
                        preds.push(pw);
                    }
                }
            }
            let mut pending = 0;
            for &p in &preds {
                let pred = &mut run.txs[p as usize];
                if pred.state != St::Final {
                    pending += 1;
                    pred.succs.push(k);
                }
            }
            for &(a, w) in &locks {
                let acct = &mut run.accts[a as usize];
                acct.lockers.push((k, w));
                if w {
                    acct.last_writer = Some(k);
                    acct.unexec_w.insert(k);
                    if certain.contains(&a) {
                        acct.unexec_certain.insert(k);
                    }
                }
            }
            run.txs.push(TxS {
                locks,
                is_vote: meta.is_vote,
                external: meta.external,
                preds,
                pending,
                succs: Vec::new(),
                state: St::Waiting,
                next_inc: 0,
                spec_incs: 0,
                running_spec: false,
                out: None,
                out_spec: false,
                write_keys: Vec::new(),
                token: 0,
                executed_once: false,
                t_ingest,
                t_first_dispatch: None,
                out_inc: 0,
                pred_recs: Vec::new(),
                rebased: 0,
                t_ready: (pending == 0).then_some(t_ingest),
            });
            new_ready.push(k);
        }
        for k in new_ready {
            self.try_make_ready(run_id, k);
        }
    }

    /// Move a Waiting transaction to Ready if its preds are final or it may speculate.
    fn try_make_ready(&mut self, run_id: RunId, k: TxIdx) {
        let speculation = self.tunables.speculation();
        let theta = self.tunables.theta();
        let max_inc = self.tunables.max_incarnations();
        let rebase = self.rebase_on();
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        let tx = &run.txs[k as usize];
        if tx.state != St::Waiting || tx.external {
            return;
        }
        let class = if tx.pending == 0 {
            if tx.is_vote { CLASS_VOTE } else { CLASS_NONSPEC }
        } else if speculation
            && tx.spec_incs < max_inc
            && Self::spec_ok(run, k, theta, &self.hints, rebase)
        {
            if rebase && !Self::spec_ok(run, k, theta, &self.hints, false) {
                run.summary.spec_relaxed += 1;
            }
            let tx = &run.txs[k as usize];
            if tx.is_vote { CLASS_VOTE } else { CLASS_SPEC }
        } else {
            return;
        };
        Self::push_ready(&mut self.heap, run, run_id, k, class);
    }

    fn push_ready(heap: &mut BinaryHeap<HeapKey>, run: &mut RunS, run_id: RunId, k: TxIdx, class: u8) {
        let tx = &mut run.txs[k as usize];
        tx.state = St::Ready;
        tx.token = tx.token.wrapping_add(1);
        heap.push(Reverse((class, run.order, k, tx.token, run_id)));
    }

    /// Delta rebase is active (it needs eager re-execution).
    fn rebase_on(&self) -> bool {
        self.tunables.rebase() && self.tunables.eager_reexec()
    }

    /// Whether every unexecuted earlier write-locker of every account `k` locks is
    /// unlikely to change it — with `rebase`, unlikely to change it in a way the delta
    /// rebase does not predict (`n * p_change * p_miss <= theta`; a certain writer such as a
    /// fee payer is allowed when the account's miss rate is low).
    fn spec_ok(run: &RunS, k: TxIdx, theta: f32, hints: &Hints, rebase: bool) -> bool {
        let tx = &run.txs[k as usize];
        for &(a, _) in &tx.locks {
            let acct = &run.accts[a as usize];
            let mut unexec = acct.unexec_w.range(..k);
            if unexec.next().is_none() {
                continue;
            }
            let miss = if rebase {
                hints.miss(&acct.key, acct.payer)
            } else {
                1.0
            };
            if acct.unexec_certain.range(..k).next().is_some()
                && !(rebase && miss <= CERTAIN_MISS_MAX)
            {
                return false;
            }
            let n = 1 + unexec.count();
            if n as f32 * hints.get(&acct.key) * miss > theta {
                return false;
            }
        }
        true
    }

    /// Send ready transactions to idle workers.
    pub fn dispatch(&mut self) {
        while self.running_total < self.workers {
            let Some(Reverse((_class, _order, k, token, run_id))) = self.heap.pop() else {
                return;
            };
            let Some(run) = self.runs.get_mut(&run_id) else {
                continue;
            };
            let tx = &mut run.txs[k as usize];
            if tx.state != St::Ready || tx.token != token {
                continue;
            }
            tx.state = St::Running;
            tx.t_first_dispatch.get_or_insert_with(Instant::now);
            tx.running_spec = tx.pending > 0;
            if tx.running_spec {
                tx.spec_incs += 1;
                run.summary.spec_dispatches += 1;
            } else {
                run.summary.nonspec_dispatches += 1;
            }
            let inc = tx.next_inc;
            tx.next_inc += 1;
            run.summary.incarnations += 1;
            let task = WorkerTask {
                run_id,
                run: Arc::clone(&run.run),
                k,
                inc,
                prev_writes: tx.write_keys.clone(),
            };
            run.running += 1;
            self.running_total += 1;
            if self.task_tx.send(task).is_err() {
                return;
            }
        }
    }

    fn on_done(&mut self, run_id: RunId, k: TxIdx, inc: u32, out: ExecOutput) {
        let eager = self.tunables.eager_reexec();
        let rebase_on = self.rebase_on();
        let max_inc = self.tunables.max_incarnations();
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        let overlay = run.run.overlay();
        let prev_keys = std::mem::take(&mut run.txs[k as usize].write_keys);
        let new_keys: Vec<Pubkey> = out.writes.iter().map(|(key, _)| *key).collect();
        {
            let tx = &mut run.txs[k as usize];
            debug_assert_eq!(tx.state, St::Running);
            debug_assert_eq!(tx.next_inc, inc + 1);
            tx.write_keys = new_keys.clone();
        }

        // Check the predictions installed for this transaction since its previous
        // incarnation (hints only): on the input a prediction assumed, did this incarnation
        // write the predicted value?
        if !run.txs[k as usize].pred_recs.is_empty() {
            let recs = std::mem::take(&mut run.txs[k as usize].pred_recs);
            for rec in recs {
                let bytes = rec.bytes();
                run.pred_bytes -= bytes;
                crate::mem::PRED_BYTES.sub(bytes);
                let same_input = out.reads.iter().any(|read| {
                    read.key == rec.key
                        && read.value.as_ref().is_some_and(|v| accounts_equal(v, &rec.basis))
                });
                if !same_input {
                    continue;
                }
                let hit = out
                    .writes
                    .iter()
                    .any(|(key, w)| *key == rec.key && accounts_equal(w, &rec.predicted));
                if hit {
                    run.summary.pred_hits += 1;
                } else {
                    run.summary.pred_misses += 1;
                }
                let payer = run
                    .acct_idx
                    .get(&rec.key)
                    .is_some_and(|&a| run.accts[a as usize].payer);
                self.hints.observe_miss(&rec.key, payer, !hit);
            }
        }

        // First execution: this transaction's effects are now visible to speculators.
        let first_exec = !run.txs[k as usize].executed_once;
        if first_exec {
            let txs = &mut run.txs;
            let accts = &mut run.accts;
            let tx = &mut txs[k as usize];
            tx.executed_once = true;
            for &(a, w) in &tx.locks {
                if w {
                    let acct = &mut accts[a as usize];
                    acct.unexec_w.remove(&k);
                    acct.unexec_certain.remove(&k);
                }
            }
        }

        // Dynamic predecessors: reads outside the lock set.
        for read in &out.reads {
            let Some(&a) = run.acct_idx.get(&read.key) else {
                continue;
            };
            if run.txs[k as usize].locks.iter().any(|&(la, _)| la == a) {
                continue;
            }
            let lockers = &run.accts[a as usize].lockers;
            let pos = lockers.partition_point(|&(t, _)| t < k);
            let pw = lockers[..pos].iter().rev().find(|&&(_, w)| w).map(|&(t, _)| t);
            if let Some(pw) = pw {
                if !run.txs[k as usize].preds.contains(&pw) {
                    run.txs[k as usize].preds.push(pw);
                    if run.txs[pw as usize].state != St::Final {
                        run.txs[k as usize].pending += 1;
                        run.txs[pw as usize].succs.push(k);
                    }
                }
            }
        }

        let pending = run.txs[k as usize].pending;
        let was_spec = run.txs[k as usize].running_spec;

        // Eager check of this incarnation's own reads while still speculative.
        let can_retry = run.txs[k as usize].spec_incs < max_inc;
        let stale = eager
            && pending > 0
            && (can_retry || rebase_on)
            && out.reads.iter().any(|read| {
                let vis = overlay.visible_below(&read.key, k);
                vis.origin != read.origin && !same_value(&vis.value, &read.value)
            });
        if stale && can_retry {
            run.summary.eager_reexecs += 1;
            let class = if run.txs[k as usize].is_vote { CLASS_VOTE } else { CLASS_SPEC };
            run.txs[k as usize].out = Some(out);
            run.txs[k as usize].out_inc = inc;
            run.txs[k as usize].out_spec = was_spec;
            Self::push_ready(&mut self.heap, run, run_id, k, class);
            if rebase_on {
                // Its outputs were computed from inputs that have moved on: predict them
                // from this incarnation's own change and let readers speculate on that.
                let mut changed = Self::repredict(run, k);
                changed.extend(new_keys.iter().chain(&prev_keys).copied());
                self.cascade(run_id, vec![(k, changed)]);
            } else {
                self.invalidate_readers(run_id, k, &new_keys, &prev_keys);
            }
            return;
        }

        {
            let tx = &mut run.txs[k as usize];
            tx.out = Some(out);
            tx.out_inc = inc;
            tx.out_spec = was_spec;
            tx.state = St::Executed;
        }

        // Wake waiting transactions whose speculation condition may have flipped.
        if first_exec && self.tunables.speculation() {
            let mut wake = Vec::new();
            for &(a, w) in &run.txs[k as usize].locks {
                if !w {
                    continue;
                }
                let lockers = &run.accts[a as usize].lockers;
                let pos = lockers.partition_point(|&(t, _)| t <= k);
                for &(r, _) in lockers[pos..].iter().take(16) {
                    if run.txs[r as usize].state == St::Waiting {
                        wake.push(r);
                    }
                }
            }
            wake.sort_unstable();
            wake.dedup();
            for r in wake {
                self.try_make_ready(run_id, r);
            }
        }

        if rebase_on {
            let Some(run) = self.runs.get_mut(&run_id) else {
                return;
            };
            // Stale but out of speculative incarnations: still predict its outputs.
            let mut changed = if stale { Self::repredict(run, k) } else { Vec::new() };
            changed.extend(new_keys.iter().chain(&prev_keys).copied());
            self.cascade(run_id, vec![(k, changed)]);
        } else if eager {
            self.invalidate_readers(run_id, k, &new_keys, &prev_keys);
        }

        if pending == 0 {
            self.worklist.push((run_id, k));
        }
    }

    /// Delta rebase of transaction `r`'s outputs: for each account its last executed
    /// incarnation wrote, whose input has changed since, install (as `r`'s version, never
    /// final) the incarnation's own change re-applied to the input now visible; where the
    /// input is back to what the incarnation read, reinstall its actual output. Only for
    /// Executed/Ready transactions (never while an incarnation runs, whose worker installs
    /// its own writes). Returns the keys whose installed version changed.
    fn repredict(run: &mut RunS, r: TxIdx) -> Vec<Pubkey> {
        let sched_run = Arc::clone(&run.run);
        let overlay = sched_run.overlay();
        let tx = &run.txs[r as usize];
        if tx.external || !matches!(tx.state, St::Executed | St::Ready) {
            return Vec::new();
        }
        let Some(out) = tx.out.as_ref() else {
            return Vec::new();
        };
        let mut changed = Vec::new();
        let mut recs = Vec::new();
        let mut installs = Vec::new();
        for (key, old_out) in &out.writes {
            let Some(Some(old_in)) = out
                .reads
                .iter()
                .find(|read| read.key == *key)
                .map(|read| read.value.as_ref())
            else {
                continue;
            };
            let vis = overlay.visible_below(key, r);
            let Some(new_in) = vis.value.as_ref() else {
                continue;
            };
            let (target, predicted) = if accounts_equal(new_in, old_in) {
                (old_out.clone(), false)
            } else {
                match rebase(old_in, old_out, new_in) {
                    Some(target) => (target, true),
                    None => continue,
                }
            };
            match overlay.version_of(key, r) {
                Some((_, current)) if !accounts_equal(&current, &target) => {}
                _ => continue,
            }
            if predicted {
                recs.push(PredRec {
                    key: *key,
                    basis: new_in.clone(),
                    predicted: target.clone(),
                });
            }
            installs.push((*key, target, predicted));
        }
        for (key, target, predicted) in installs {
            let inc = if predicted {
                run.pred_seq = (run.pred_seq + 1) & !PRED_INC_BIT;
                PRED_INC_BIT | run.pred_seq
            } else {
                // Restoring the executed value: its prediction record is void.
                let tx = &mut run.txs[r as usize];
                if let Some(i) = tx.pred_recs.iter().position(|rec| rec.key == key) {
                    let bytes = tx.pred_recs.swap_remove(i).bytes();
                    run.pred_bytes -= bytes;
                    crate::mem::PRED_BYTES.sub(bytes);
                }
                run.txs[r as usize].out_inc
            };
            overlay.install(r, inc, &[(key, target)], &[]);
            changed.push(key);
        }
        if !recs.is_empty() {
            run.summary.predictions += recs.len() as u64;
            let tx = &mut run.txs[r as usize];
            tx.rebased += recs.len() as u32;
            for rec in recs {
                let bytes = rec.bytes();
                run.pred_bytes += bytes;
                crate::mem::PRED_BYTES.add(bytes);
                if let Some(old) = tx.pred_recs.iter_mut().find(|old| old.key == rec.key) {
                    let old_bytes = old.bytes();
                    run.pred_bytes -= old_bytes;
                    crate::mem::PRED_BYTES.sub(old_bytes);
                    *old = rec;
                } else {
                    tx.pred_recs.push(rec);
                }
            }
        }
        changed
    }

    /// Eager invalidation with delta rebase: the versions of `keys` written by transaction
    /// `j` changed. Every later locker up to the next writer of each key whose last executed
    /// incarnation read a now-stale value is re-dispatched (as [`Self::invalidate_readers`])
    /// and its outputs are re-predicted; readers of re-predicted outputs are processed in
    /// turn, lowest transaction first (a DAG: each step moves to later transactions).
    fn cascade(&mut self, run_id: RunId, seeds: Vec<(TxIdx, Vec<Pubkey>)>) {
        let max_inc = self.tunables.max_incarnations();
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        let sched_run = Arc::clone(&run.run);
        let overlay = sched_run.overlay();
        let mut work: BTreeMap<TxIdx, Vec<Pubkey>> = BTreeMap::new();
        for (j, keys) in seeds {
            work.entry(j).or_default().extend(keys);
        }
        let mut budget = CASCADE_BUDGET;
        while let Some((j, mut keys)) = work.pop_first() {
            keys.sort_unstable();
            keys.dedup();
            let mut stale_readers = BTreeSet::new();
            for key in &keys {
                let Some(&a) = run.acct_idx.get(key) else {
                    continue;
                };
                let lockers = &run.accts[a as usize].lockers;
                let pos = lockers.partition_point(|&(t, _)| t <= j);
                for &(r, w) in &lockers[pos..] {
                    if budget == 0 {
                        break;
                    }
                    budget -= 1;
                    let tx = &run.txs[r as usize];
                    if matches!(tx.state, St::Executed | St::Ready) && !tx.external {
                        if let Some(read) = tx
                            .out
                            .as_ref()
                            .and_then(|out| out.reads.iter().find(|read| &read.key == key))
                        {
                            let vis = overlay.visible_below(key, r);
                            if vis.origin != read.origin && !same_value(&vis.value, &read.value) {
                                stale_readers.insert((r, true));
                            } else if !tx.pred_recs.is_empty() {
                                // Input back to what it read: undo its predictions.
                                stale_readers.insert((r, false));
                            }
                        }
                    }
                    if w {
                        break;
                    }
                }
            }
            for (r, stale) in stale_readers {
                let tx = &run.txs[r as usize];
                if stale && tx.state == St::Executed && tx.pending > 0 && tx.spec_incs < max_inc {
                    run.summary.eager_reexecs += 1;
                    let class = if tx.is_vote { CLASS_VOTE } else { CLASS_SPEC };
                    Self::push_ready(&mut self.heap, run, run_id, r, class);
                }
                let changed = Self::repredict(run, r);
                if !changed.is_empty() {
                    work.entry(r).or_default().extend(changed);
                }
            }
        }
    }

    /// Re-dispatch executed, not-yet-final readers of accounts whose visible version just
    /// changed because of `k` (bounded to the next write-locker of each account).
    fn invalidate_readers(
        &mut self,
        run_id: RunId,
        k: TxIdx,
        new_keys: &[Pubkey],
        prev_keys: &[Pubkey],
    ) {
        let max_inc = self.tunables.max_incarnations();
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        let overlay = run.run.overlay();
        let mut redispatch = Vec::new();
        for key in new_keys.iter().chain(prev_keys) {
            let Some(&a) = run.acct_idx.get(key) else {
                continue;
            };
            let lockers = &run.accts[a as usize].lockers;
            let pos = lockers.partition_point(|&(t, _)| t <= k);
            for &(r, w) in &lockers[pos..] {
                let tx = &run.txs[r as usize];
                if tx.state == St::Executed && tx.pending > 0 && tx.spec_incs < max_inc {
                    if let Some(out) = &tx.out {
                        if let Some(read) = out.reads.iter().find(|read| &read.key == key) {
                            let vis = overlay.visible_below(key, r);
                            if vis.origin != read.origin && !same_value(&vis.value, &read.value) {
                                redispatch.push(r);
                            }
                        }
                    }
                }
                if w {
                    break;
                }
            }
        }
        redispatch.sort_unstable();
        redispatch.dedup();
        for r in redispatch {
            run.summary.eager_reexecs += 1;
            let class = if run.txs[r as usize].is_vote { CLASS_VOTE } else { CLASS_SPEC };
            Self::push_ready(&mut self.heap, run, run_id, r, class);
        }
    }

    fn drain_worklist(&mut self) {
        while let Some((run_id, k)) = self.worklist.pop() {
            self.finalize(run_id, k);
        }
    }

    /// `k` is executed and all its preds are final: validate and emit, or retry.
    fn finalize(&mut self, run_id: RunId, k: TxIdx) {
        let rebase_on = self.rebase_on();
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        if run.txs[k as usize].state != St::Executed || run.txs[k as usize].pending != 0 {
            return;
        }
        let overlay = run.run.overlay();
        let valid = match &run.txs[k as usize].out {
            Some(out) => out.reads.iter().all(|read| {
                let fv = overlay.final_below(&read.key, k);
                fv.origin == read.origin || same_value(&fv.value, &read.value)
            }),
            None => false,
        };
        if !valid {
            run.summary.validation_failures += 1;
            if rebase_on {
                // Its inputs are final now: keep its versions as predictions, rebased onto
                // the final inputs, while it re-executes (it cannot become final before).
                let changed = Self::repredict(run, k);
                Self::push_ready(&mut self.heap, run, run_id, k, CLASS_RETRY);
                self.cascade(run_id, vec![(k, changed)]);
                return;
            }
            let keys = std::mem::take(&mut run.txs[k as usize].write_keys);
            for key in &keys {
                overlay.remove_version(key, k);
            }
            Self::push_ready(&mut self.heap, run, run_id, k, CLASS_RETRY);
            if self.tunables.eager_reexec() {
                self.invalidate_readers(run_id, k, &[], &keys);
            }
            return;
        }

        // FINAL.
        let t_final = Instant::now();
        let (out, out_inc, out_spec, incarnations, succs, t_ingest, t_first_dispatch, n_preds) = {
            let tx = &mut run.txs[k as usize];
            tx.state = St::Final;
            (
                tx.out.take(),
                tx.out_inc,
                tx.out_spec,
                tx.next_inc,
                std::mem::take(&mut tx.succs),
                tx.t_ingest,
                tx.t_first_dispatch.unwrap_or(tx.t_ingest),
                tx.preds.len(),
            )
        };
        let (t_ready, rebased) = {
            let tx = &mut run.txs[k as usize];
            let recs = std::mem::take(&mut tx.pred_recs);
            let bytes: i64 = recs.iter().map(PredRec::bytes).sum();
            run.pred_bytes -= bytes;
            crate::mem::PRED_BYTES.sub(bytes);
            (tx.t_ready.unwrap_or(t_ingest), tx.rebased)
        };
        run.n_final += 1;
        run.summary.finals += 1;
        let Some(out) = out else {
            overlay.mark_final(k, &run.txs[k as usize].write_keys);
            return;
        };
        // The final versions are exactly the validated incarnation's writes (a prediction
        // installed after it ran is replaced here; never the case without rebase).
        let fixed = overlay.commit_final(k, out_inc, &out.writes, &run.txs[k as usize].write_keys);
        run.summary.final_fixups += fixed.len() as u64;
        if out.unprocessable {
            run.summary.unprocessable += 1;
        }
        // Hints: did each write-locked account actually change? Writes are few; reads are
        // looked up only for written keys.
        for &(a, w) in &run.txs[k as usize].locks {
            if !w {
                continue;
            }
            let key = &run.accts[a as usize].key;
            let changed = match out.writes.iter().find(|(wk, _)| wk == key) {
                None => false,
                Some((_, account)) => {
                    match out.reads.iter().find(|read| &read.key == key).map(|r| &r.value) {
                        Some(Some(read)) => !crate::mv::accounts_equal(account, read),
                        Some(None) => !account.lamports_is_zero(),
                        None => true,
                    }
                }
            };
            self.hints.observe(key, changed);
        }
        let ExecOutput {
            payload,
            exec_start,
            exec_end,
            ..
        } = out;
        self.sink.on_final(Finalized {
            run_id,
            k,
            incarnations,
            speculative: out_spec,
            payload,
            t_ingest,
            t_first_dispatch,
            t_ready,
            rebased,
            t_exec_start: exec_start,
            t_exec_end: exec_end,
            t_final,
            n_preds,
        });
        // Readers of a replaced prediction: re-dispatch/re-predict them now rather than at
        // their own validation.
        if rebase_on && !fixed.is_empty() {
            self.cascade(run_id, vec![(k, fixed)]);
        }
        // Release successors.
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        for s in succs {
            let tx = &mut run.txs[s as usize];
            if tx.state == St::Final {
                continue;
            }
            tx.pending -= 1;
            if tx.pending == 0 {
                tx.t_ready = Some(t_final);
                match tx.state {
                    St::Executed => self.worklist.push((run_id, s)),
                    St::Waiting if tx.external => {}
                    St::Waiting => {
                        let class = if tx.is_vote { CLASS_VOTE } else { CLASS_NONSPEC };
                        Self::push_ready(&mut self.heap, run, run_id, s, class);
                    }
                    St::Ready => {
                        // Re-queue at non-speculative priority (old entry becomes stale).
                        let class = if tx.is_vote { CLASS_VOTE } else { CLASS_NONSPEC };
                        Self::push_ready(&mut self.heap, run, run_id, s, class);
                    }
                    St::Running | St::Final => {}
                }
            }
        }
    }

    fn maybe_end_run(&mut self, run_id: RunId) {
        let done = match self.runs.get(&run_id) {
            Some(run) => {
                run.total.is_some_and(|total| run.txs.len() as TxIdx == total)
                    && run.n_final as usize == run.txs.len()
                    && run.running == 0
            }
            None => false,
        };
        if done {
            if let Some(mut run) = self.runs.remove(&run_id) {
                run.summary.txs = run.txs.len() as u64;
                run.run.on_complete(&run.summary);
                self.sink.on_run_end(run_id, std::mem::take(&mut run.summary));
            }
        }
    }

    /// Diagnostic: (waiting, ready, running, executed, final) counts of a run.
    pub fn run_state_counts(&self, run_id: RunId) -> Option<[usize; 5]> {
        let run = self.runs.get(&run_id)?;
        let mut c = [0usize; 5];
        for tx in &run.txs {
            let i = match tx.state {
                St::Waiting => 0,
                St::Ready => 1,
                St::Running => 2,
                St::Executed => 3,
                St::Final => 4,
            };
            c[i] += 1;
        }
        Some(c)
    }
}

trait LamportsIsZero {
    fn lamports_is_zero(&self) -> bool;
}
impl LamportsIsZero for AccountSharedData {
    fn lamports_is_zero(&self) -> bool {
        solana_account::ReadableAccount::lamports(self) == 0
    }
}

#[allow(dead_code)]
fn _origin_is_base(o: Origin) -> bool {
    o == Origin::Base
}

#[cfg(test)]
mod tests;
