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

use {
    crate::{
        control::Tunables,
        mv::{Origin, Overlay, Read, TxIdx, same_value},
    },
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError},
    log::warn,
    solana_account::AccountSharedData,
    solana_pubkey::Pubkey,
    std::{
        any::Any,
        cmp::Reverse,
        collections::{BTreeSet, BinaryHeap, HashMap},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    },
};

/// A run the scheduler can execute: an overlay plus a way to execute one incarnation.
pub trait SchedRun: Send + Sync + 'static {
    fn overlay(&self) -> &Overlay;
    /// Execute incarnation `inc` of transaction `k`, reading through the overlay. Must not
    /// install anything; the executor installs `writes` afterwards.
    fn execute(&self, k: TxIdx, inc: u32) -> ExecOutput;
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
    pub unprocessable: u64,
}

/// Receives FINAL transactions and run ends (runs on the coordinator thread).
pub trait FinalSink: Send {
    fn on_final(&mut self, finalized: Finalized);
    fn on_run_end(&mut self, run_id: RunId, summary: RunSummary);
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
}

struct AcctS {
    key: Pubkey,
    lockers: Vec<(TxIdx, bool)>,
    last_writer: Option<TxIdx>,
    unexec_w: BTreeSet<TxIdx>,
    unexec_certain: BTreeSet<TxIdx>,
}

struct RunS {
    run: Arc<dyn SchedRun>,
    order: u64,
    txs: Vec<TxS>,
    acct_idx: HashMap<Pubkey, u32>,
    accts: Vec<AcctS>,
    n_final: u32,
    total: Option<TxIdx>,
    running: u32,
    summary: RunSummary,
}

/// Priority class: lower runs first.
const CLASS_RETRY: u8 = 0;
const CLASS_NONSPEC: u8 = 1;
const CLASS_SPEC: u8 = 2;
const CLASS_VOTE: u8 = 3;

type HeapKey = Reverse<(u8, u64, TxIdx, u32, RunId)>;

/// Global per-account change-probability hints (EWMA over FINAL write-lockers).
pub struct Hints {
    phat: HashMap<Pubkey, f32>,
    alpha: f32,
    prior: f32,
    cap: usize,
}

impl Hints {
    pub fn new(alpha: f32) -> Self {
        Self {
            phat: HashMap::new(),
            alpha,
            prior: 0.5,
            cap: 262_144,
        }
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
    pub fn is_empty(&self) -> bool {
        self.phat.is_empty()
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

/// Receive with a bounded busy-poll before parking; `None` on exit/disconnect.
pub fn recv_spin<T>(rx: &Receiver<T>, spin: Duration, exit: &AtomicBool) -> Option<T> {
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
        loop {
            let Some(msg) = recv_spin(&rx, spin, &exit) else {
                return;
            };
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
                        acct_idx: HashMap::new(),
                        accts: Vec::new(),
                        n_final: 0,
                        total: None,
                        running: 0,
                        summary: RunSummary::default(),
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
                    self.sink.on_run_end(run_id, run.summary);
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
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        let tx = &run.txs[k as usize];
        if tx.state != St::Waiting {
            return;
        }
        let class = if tx.pending == 0 {
            if tx.is_vote { CLASS_VOTE } else { CLASS_NONSPEC }
        } else if speculation
            && tx.spec_incs < max_inc
            && Self::spec_ok(run, k, theta, &self.hints)
        {
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

    /// Whether every unexecuted earlier write-locker of every account `k` locks is
    /// unlikely to change it.
    fn spec_ok(run: &RunS, k: TxIdx, theta: f32, hints: &Hints) -> bool {
        let tx = &run.txs[k as usize];
        for &(a, _) in &tx.locks {
            let acct = &run.accts[a as usize];
            let mut unexec = acct.unexec_w.range(..k);
            if unexec.next().is_none() {
                continue;
            }
            if acct.unexec_certain.range(..k).next().is_some() {
                return false;
            }
            let n = 1 + unexec.count();
            if n as f32 * hints.get(&acct.key) > theta {
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
        if out.unprocessable {
            run.summary.unprocessable += 1;
        }

        // First execution: this transaction's effects are now visible to speculators.
        let first_exec = !run.txs[k as usize].executed_once;
        if first_exec {
            run.txs[k as usize].executed_once = true;
            let locks = run.txs[k as usize].locks.clone();
            for &(a, w) in &locks {
                if w {
                    let acct = &mut run.accts[a as usize];
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
        if eager && pending > 0 && run.txs[k as usize].spec_incs < max_inc {
            let stale = out.reads.iter().any(|read| {
                let vis = overlay.visible_below(&read.key, k);
                vis.origin != read.origin && !same_value(&vis.value, &read.value)
            });
            if stale {
                run.summary.eager_reexecs += 1;
                let class = if run.txs[k as usize].is_vote { CLASS_VOTE } else { CLASS_SPEC };
                run.txs[k as usize].out = Some(out);
                run.txs[k as usize].out_spec = was_spec;
                Self::push_ready(&mut self.heap, run, run_id, k, class);
                self.invalidate_readers(run_id, k, &new_keys, &prev_keys);
                return;
            }
        }

        {
            let tx = &mut run.txs[k as usize];
            tx.out = Some(out);
            tx.out_spec = was_spec;
            tx.state = St::Executed;
        }

        // Wake waiting transactions whose speculation condition may have flipped.
        if first_exec && self.tunables.speculation() {
            let locks = run.txs[k as usize].locks.clone();
            let mut wake = Vec::new();
            for &(a, w) in &locks {
                if !w {
                    continue;
                }
                let lockers = &run.accts[a as usize].lockers;
                let pos = lockers.partition_point(|&(t, _)| t <= k);
                for &(r, _) in lockers[pos..].iter().take(64) {
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

        if eager {
            self.invalidate_readers(run_id, k, &new_keys, &prev_keys);
        }

        if pending == 0 {
            self.worklist.push((run_id, k));
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
        let (out, out_spec, incarnations, write_keys, locks, succs, t_ingest, n_preds) = {
            let tx = &mut run.txs[k as usize];
            tx.state = St::Final;
            (
                tx.out.take(),
                tx.out_spec,
                tx.next_inc,
                tx.write_keys.clone(),
                tx.locks.clone(),
                std::mem::take(&mut tx.succs),
                tx.t_ingest,
                tx.preds.len(),
            )
        };
        run.n_final += 1;
        run.summary.finals += 1;
        overlay.mark_final(k, &write_keys);
        let Some(out) = out else {
            return;
        };
        // Hints: did each write-locked account actually change?
        for &(a, w) in &locks {
            if !w {
                continue;
            }
            let key = run.accts[a as usize].key;
            let read_value = out
                .reads
                .iter()
                .find(|read| read.key == key)
                .map(|read| &read.value);
            let written = out.writes.iter().find(|(wk, _)| *wk == key);
            let changed = match (written, read_value) {
                (None, _) => false,
                (Some((_, account)), Some(read_value)) => {
                    !same_value(&Some(account.clone()), read_value)
                        && !(account.lamports_is_zero() && read_value.is_none())
                }
                (Some(_), None) => true,
            };
            self.hints.observe(&key, changed);
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
            t_exec_start: exec_start,
            t_exec_end: exec_end,
            t_final,
            n_preds,
        });
        // Release successors.
        let run = self.runs.get_mut(&run_id).expect("run exists");
        for s in succs {
            let tx = &mut run.txs[s as usize];
            if tx.state == St::Final {
                continue;
            }
            tx.pending -= 1;
            if tx.pending == 0 {
                match tx.state {
                    St::Executed => self.worklist.push((run_id, s)),
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
                self.sink.on_run_end(run_id, run.summary);
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
