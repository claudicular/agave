//! Execute-once milestone 2 (`commit = on`): FL commits its FINAL transactions into agave's
//! bank N through agave's own commit path (`solana_runtime::transaction_execution::
//! commit_external`), and agave's replay of N follows (`solana_runtime::fast_lane_commit`).
//!
//! The [`Committer`] runs on the coordinator thread (inside the FINAL sink). For each run it
//! binds to agave's bank of the run's slot once agave inserted it into bank forks (same
//! parent slot and bank id as the run), takes each FINAL transaction's full processing result
//! and hands it to a commit thread as soon as the transaction's commit-order predecessors
//! (`Finalized::cpreds`: every earlier transaction it conflicts with the way agave's
//! scheduler orders them) are in the bank, committed by FL or by agave. Commits of
//! non-conflicting transactions run in parallel on `commit_threads` threads.
//!
//! FL declines (agave executes) what it must not commit: transactions touching a program
//! loader, results that modified programs, unprocessable results, results executed without
//! agave's recording configuration, and the verification sample (whose FL result is
//! deposited for agave's handler to compare before committing its own).

use {
    crate::{
        ReplayServices,
        control,
        mem,
        mv::TxIdx,
        run::{FlProcessed, OutcomeKind, Run, TxEntry, TxOutcome},
        sched::{CoordMsg, RunId, RunSummary, recv_spin},
    },
    crossbeam_channel::{Receiver, Sender},
    log::{info, warn},
    solana_clock::{BankId, Slot},
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        fast_lane_commit::{self, Board, SampleDeposit},
        transaction_execution::{CommitServices, ExternalCommit, commit_external},
    },
    solana_svm::transaction_processing_result::ProcessedTransaction,
    solana_svm_timings::ExecuteTimings,
    std::{
        collections::{HashMap, VecDeque},
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{
            Arc, Mutex, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    },
};

/// Events for the committer (sent through the coordinator channel as `CoordMsg::Sink`).
pub enum CommitEvent {
    /// Ingest started a run (sent right after `CoordMsg::NewRun`).
    RunBegin { run_id: RunId, run: Arc<Run> },
    /// Agave inserted a bank into bank forks.
    BankInserted(Arc<Bank>),
    /// Agave's replay executed and committed transaction `index` of `bank_id` itself.
    AgaveDone { bank_id: BankId, index: usize },
    /// A commit thread finished a job.
    Done {
        run_id: RunId,
        k: TxIdx,
        outcome: CommitOutcome,
        t_final: Instant,
        t_done: Instant,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    Committed(bool),
    Declined(&'static str),
    Lost,
    /// Not attempted: FL stopped committing (disabled, poisoned, mode changed).
    Stopped,
    /// The commit unwound (the cell is failed and the slot marked for replay).
    Failed,
}

/// One transaction to commit.
pub struct CommitJob {
    pub run_id: RunId,
    pub k: TxIdx,
    pub index: usize,
    pub bank: Arc<Bank>,
    pub board: Arc<Board>,
    pub tx: Arc<TxEntry>,
    pub processed: Box<FlProcessed>,
    pub t_final: Instant,
    pub bytes: i64,
}

/// Why FL declined a transaction (agave executes it).
pub const DECLINE_CLASSES: [&str; 7] = [
    "loader",
    "programs",
    "unprocessable",
    "unrecorded",
    "no_result",
    "sample",
    "check",
];

/// Commit counters shared with the comparator (cumulative; the summary reports deltas).
#[derive(Default)]
pub struct CommitMetrics {
    pub committed: AtomicU64,
    pub committed_err: AtomicU64,
    pub declined: [AtomicU64; 7],
    pub lost: AtomicU64,
    pub stopped: AtomicU64,
    pub failed: AtomicU64,
    pub runs_bound: AtomicU64,
    pub runs_unbound: AtomicU64,
    pub bank_waits: AtomicU64,
    pub deposits: AtomicU64,
    /// Commit latency (FINAL → committed), bank wait (a FINAL tx waiting for bank N), and
    /// commit service time (per job on the commit thread), µs samples of the interval.
    pub latency_us: Mutex<Vec<u32>>,
    pub bank_wait_us: Mutex<Vec<u32>>,
    pub service_us: Mutex<Vec<u32>>,
    pub max_waiting: AtomicU64,
}

impl CommitMetrics {
    fn decline(&self, class: &'static str) {
        if let Some(i) = DECLINE_CLASSES.iter().position(|c| *c == class) {
            self.declined[i].fetch_add(1, Ordering::Relaxed);
        }
    }

    fn sample(vec: &Mutex<Vec<u32>>, us: u64) {
        if let Ok(mut v) = vec.lock() {
            if v.len() < 1 << 18 {
                v.push(us.min(u64::from(u32::MAX)) as u32);
            }
        }
    }
}

struct Pending {
    cpreds: Vec<TxIdx>,
    tx: Arc<TxEntry>,
    processed: Box<FlProcessed>,
    t_final: Instant,
    bytes: i64,
    /// The predecessor it is registered as waiting for (in `CommitRun::waiters`).
    parked_on: Option<TxIdx>,
}

struct CommitRun {
    run: Arc<Run>,
    base: TxIdx,
    bank: Option<Arc<Bank>>,
    board: Option<Arc<Board>>,
    waiting: HashMap<TxIdx, Pending>,
    /// Waiting transactions keyed by a predecessor they wait for.
    waiters: HashMap<TxIdx, Vec<TxIdx>>,
    /// Declined before the run was bound (applied at binding), with FL's result for the
    /// verification sample.
    declines: Vec<(TxIdx, Option<Box<FlProcessed>>)>,
    in_flight: u32,
    first_final: Option<Instant>,
    ended: bool,
    /// Transactions of the complete run (agave's indexes).
    total: Option<usize>,
    started: Instant,
}

impl CommitRun {
    fn pending_bytes(&self) -> i64 {
        self.waiting.values().map(|p| p.bytes).sum()
    }
}

pub struct Committer {
    job_tx: Sender<CommitJob>,
    bank_forks: Option<Arc<RwLock<BankForks>>>,
    runs: HashMap<RunId, CommitRun>,
    by_bank: HashMap<BankId, RunId>,
    recent_banks: VecDeque<(Instant, Arc<Bank>)>,
    pub metrics: Arc<CommitMetrics>,
    last_sweep: Instant,
    released: bool,
}

fn pending_bytes(processed: &FlProcessed) -> i64 {
    crate::full_cmp::processing_result_bytes(&processed.result) + 512
}

impl Committer {
    pub fn new(
        job_tx: Sender<CommitJob>,
        bank_forks: Option<Arc<RwLock<BankForks>>>,
        metrics: Arc<CommitMetrics>,
    ) -> Self {
        Self {
            job_tx,
            bank_forks,
            runs: HashMap::new(),
            by_bank: HashMap::new(),
            recent_banks: VecDeque::new(),
            metrics,
            last_sweep: Instant::now(),
            released: false,
        }
    }

    pub fn active_runs(&self) -> usize {
        self.runs.len()
    }

    pub fn on_event(&mut self, event: CommitEvent) {
        match event {
            CommitEvent::RunBegin { run_id, run } => self.on_run_begin(run_id, run),
            CommitEvent::BankInserted(bank) => self.on_bank(bank),
            CommitEvent::AgaveDone { bank_id, index } => {
                if let Some(&run_id) = self.by_bank.get(&bank_id) {
                    let base = self.runs.get(&run_id).map(|r| r.base).unwrap_or(0);
                    self.wake(run_id, base + index as TxIdx);
                }
            }
            CommitEvent::Done {
                run_id,
                k,
                outcome,
                t_final,
                t_done,
            } => {
                let m = &self.metrics;
                match &outcome {
                    CommitOutcome::Committed(ok) => {
                        m.committed.fetch_add(1, Ordering::Relaxed);
                        if !ok {
                            m.committed_err.fetch_add(1, Ordering::Relaxed);
                        }
                        CommitMetrics::sample(
                            &m.latency_us,
                            t_done.saturating_duration_since(t_final).as_micros() as u64,
                        );
                    }
                    CommitOutcome::Declined(class) => m.decline(class),
                    CommitOutcome::Lost => {
                        m.lost.fetch_add(1, Ordering::Relaxed);
                    }
                    CommitOutcome::Stopped => {
                        m.stopped.fetch_add(1, Ordering::Relaxed);
                    }
                    CommitOutcome::Failed => {
                        m.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if let Some(run) = self.runs.get_mut(&run_id) {
                    run.in_flight = run.in_flight.saturating_sub(1);
                }
                self.wake(run_id, k);
            }
        }
    }

    fn on_run_begin(&mut self, run_id: RunId, run: Arc<Run>) {
        if !control::committing() {
            return;
        }
        let base = run.ordinal_base;
        self.runs.insert(
            run_id,
            CommitRun {
                run,
                base,
                bank: None,
                board: None,
                waiting: HashMap::new(),
                waiters: HashMap::new(),
                declines: Vec::new(),
                in_flight: 0,
                first_final: None,
                ended: false,
                total: None,
                started: Instant::now(),
            },
        );
        self.released = false;
        // Bank N may exist already (typical: the parent froze before N's first shred).
        let slot = self.runs[&run_id].run.slot;
        let existing = self
            .bank_forks
            .as_ref()
            .and_then(|forks| forks.read().ok()?.get(slot));
        let recent = self
            .recent_banks
            .iter()
            .rev()
            .find(|(_, b)| b.slot() == slot)
            .map(|(_, b)| Arc::clone(b));
        for bank in existing.into_iter().chain(recent) {
            if self.try_bind(run_id, &bank) {
                break;
            }
        }
    }

    fn on_bank(&mut self, bank: Arc<Bank>) {
        let now = Instant::now();
        while self
            .recent_banks
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(10))
            || self.recent_banks.len() > 64
        {
            self.recent_banks.pop_front();
        }
        let candidates: Vec<RunId> = self
            .runs
            .iter()
            .filter(|(_, r)| r.board.is_none() && r.run.slot == bank.slot())
            .map(|(id, _)| *id)
            .collect();
        for run_id in candidates {
            if self.try_bind(run_id, &bank) {
                return;
            }
        }
        self.recent_banks.push_back((now, bank));
    }

    /// Bind `run_id` to `bank` if it is the bank of the run's slot over the run's parent.
    fn try_bind(&mut self, run_id: RunId, bank: &Arc<Bank>) -> bool {
        let Some(cr) = self.runs.get_mut(&run_id) else {
            return false;
        };
        if cr.board.is_some()
            || bank.slot() != cr.run.slot
            || bank.parent_slot() != cr.run.parent_slot
            || bank.parent().map(|p| p.bank_id()) != Some(cr.run.parent_bank_id)
            || bank.is_frozen()
        {
            return false;
        }
        let Some(board) = fast_lane_commit::board_of(bank.bank_id()) else {
            return false;
        };
        if !board.bind() {
            return false;
        }
        let now = Instant::now();
        if let Some(first) = cr.first_final {
            self.metrics.bank_waits.fetch_add(1, Ordering::Relaxed);
            CommitMetrics::sample(
                &self.metrics.bank_wait_us,
                now.saturating_duration_since(first).as_micros() as u64,
            );
        }
        self.metrics.runs_bound.fetch_add(1, Ordering::Relaxed);
        for (k, processed) in std::mem::take(&mut cr.declines) {
            let index = (k - cr.base) as usize;
            if let Some(processed) = processed {
                mem::COMMIT_PENDING_BYTES.sub(pending_bytes(&processed));
                if let Some(tx) = cr.run.tx(k) {
                    if fast_lane_commit::sampled(&tx.signature, board.sample_ppm) {
                        deposit(&board, index, *processed, &self.metrics);
                        continue;
                    }
                }
            }
            board.decline(index);
        }
        if let Some(total) = cr.total {
            board.set_fl_total(total);
        }
        cr.bank = Some(Arc::clone(bank));
        cr.board = Some(board);
        self.by_bank.insert(bank.bank_id(), run_id);
        let waiting: Vec<TxIdx> = cr.waiting.keys().copied().collect();
        for k in waiting {
            self.try_dispatch(run_id, k);
        }
        true
    }

    /// A transaction became FINAL (on the coordinator thread, before the comparator and the
    /// output ring see it). Takes its processing result when FL will commit it.
    pub fn on_final(
        &mut self,
        run_id: RunId,
        k: TxIdx,
        cpreds: &[TxIdx],
        outcome: &mut TxOutcome,
        t_final: Instant,
    ) {
        if !control::committing() {
            return;
        }
        let Some(cr) = self.runs.get_mut(&run_id) else {
            return;
        };
        if k < cr.base {
            return;
        }
        cr.first_final.get_or_insert(t_final);
        let Some(tx) = cr.run.tx(k) else {
            return;
        };
        let class = if fast_lane_commit::static_exclusion(&tx.rtx).is_some() {
            Some("loader")
        } else {
            match outcome.processed.as_deref() {
                None => Some("no_result"),
                Some(p)
                    if p.check.is_err()
                        || p.result.is_err()
                        || outcome.kind == OutcomeKind::Unprocessable =>
                {
                    Some("unprocessable")
                }
                Some(FlProcessed {
                    result: Ok(ProcessedTransaction::Executed(executed)),
                    ..
                }) if !executed.programs_modified_by_tx.is_empty() => Some("programs"),
                Some(p) if control::agave_execution().record && !p.recorded => Some("unrecorded"),
                _ => None,
            }
        };
        if let Some(class) = class {
            self.metrics.decline(class);
            let index = (k - cr.base) as usize;
            match &cr.board {
                Some(board) => {
                    // A sampled transaction gets FL's result deposited whatever the reason
                    // FL does not commit it (agave's handler compares it).
                    match outcome.processed.take() {
                        Some(processed)
                            if fast_lane_commit::sampled(&tx.signature, board.sample_ppm) =>
                        {
                            deposit(board, index, *processed, &self.metrics);
                        }
                        _ => {
                            board.decline(index);
                        }
                    }
                }
                None => {
                    let processed = outcome.processed.take();
                    if let Some(p) = &processed {
                        mem::COMMIT_PENDING_BYTES.add(pending_bytes(p));
                    }
                    cr.declines.push((k, processed));
                }
            }
            return;
        }
        let Some(processed) = outcome.processed.take() else {
            return;
        };
        let bytes = pending_bytes(&processed);
        mem::COMMIT_PENDING_BYTES.add(bytes);
        cr.waiting.insert(
            k,
            Pending {
                cpreds: cpreds.to_vec(),
                tx,
                processed,
                t_final,
                bytes,
                parked_on: None,
            },
        );
        let waiting = cr.waiting.len() as u64;
        self.metrics.max_waiting.fetch_max(waiting, Ordering::Relaxed);
        self.try_dispatch(run_id, k);
    }

    /// Dispatch waiting transaction `k` if the run is bound and its commit-order
    /// predecessors are in the bank; else park it on the first missing predecessor.
    fn try_dispatch(&mut self, run_id: RunId, k: TxIdx) {
        let Some(cr) = self.runs.get_mut(&run_id) else {
            return;
        };
        let (Some(board), Some(bank)) = (cr.board.clone(), cr.bank.clone()) else {
            return;
        };
        let Some(pending) = cr.waiting.get(&k) else {
            return;
        };
        let base = cr.base;
        let index = (k - base) as usize;
        if fast_lane_commit::sampled(&pending.tx.signature, board.sample_ppm) {
            // Agave executes it and compares with this result before committing its own.
            let pending = cr.waiting.remove(&k).unwrap();
            mem::COMMIT_PENDING_BYTES.sub(pending.bytes);
            self.metrics.decline("sample");
            deposit(&board, index, *pending.processed, &self.metrics);
            return;
        }
        if let Some(&missing) = pending
            .cpreds
            .iter()
            .find(|&&j| j >= base && !board.is_done((j - base) as usize))
        {
            if pending.parked_on != Some(missing) {
                cr.waiters.entry(missing).or_default().push(k);
                if let Some(pending) = cr.waiting.get_mut(&k) {
                    pending.parked_on = Some(missing);
                }
            }
            return;
        }
        let pending = cr.waiting.remove(&k).unwrap();
        cr.in_flight += 1;
        let job = CommitJob {
            run_id,
            k,
            index,
            bank,
            board,
            tx: pending.tx,
            processed: pending.processed,
            t_final: pending.t_final,
            bytes: pending.bytes,
        };
        if self.job_tx.send(job).is_err() {
            warn!("fast lane: commit threads are gone");
        }
    }

    /// `j` is (maybe) in the bank now: re-check the transactions waiting for it.
    fn wake(&mut self, run_id: RunId, j: TxIdx) {
        let Some(cr) = self.runs.get_mut(&run_id) else {
            return;
        };
        let Some(waiters) = cr.waiters.remove(&j) else {
            return;
        };
        for &k in &waiters {
            if let Some(pending) = cr.waiting.get_mut(&k) {
                if pending.parked_on == Some(j) {
                    pending.parked_on = None;
                }
            }
        }
        for k in waiters {
            self.try_dispatch(run_id, k);
        }
    }

    pub fn on_run_end(&mut self, run_id: RunId, summary: &RunSummary) {
        let Some(cr) = self.runs.get_mut(&run_id) else {
            return;
        };
        cr.ended = true;
        if summary.aborted.is_none() {
            cr.total = Some((summary.txs as TxIdx).saturating_sub(cr.base) as usize);
            if let (Some(board), Some(total)) = (&cr.board, cr.total) {
                board.set_fl_total(total);
            }
        }
        if summary.aborted.is_some() {
            self.drop_run(run_id, true);
        }
    }

    /// Stop committing into this run's bank: agave's handlers take over its free cells.
    fn drop_run(&mut self, run_id: RunId, abandon: bool) {
        let Some(cr) = self.runs.remove(&run_id) else {
            return;
        };
        mem::COMMIT_PENDING_BYTES.sub(cr.pending_bytes());
        let decline_bytes: i64 = cr
            .declines
            .iter()
            .filter_map(|(_, p)| p.as_deref().map(pending_bytes))
            .sum();
        mem::COMMIT_PENDING_BYTES.sub(decline_bytes);
        if let Some(board) = &cr.board {
            if abandon {
                board.abandon();
            }
            self.by_bank.remove(&board.bank_id);
        } else {
            self.metrics.runs_unbound.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// About every millisecond (coordinator loop).
    pub fn tick(&mut self) {
        if !control::committing() {
            if !self.released {
                let ids: Vec<RunId> = self.runs.keys().copied().collect();
                for id in ids {
                    self.drop_run(id, true);
                }
                self.recent_banks.clear();
                self.released = true;
            }
            return;
        }
        if self.last_sweep.elapsed() < Duration::from_millis(5) {
            return;
        }
        self.last_sweep = Instant::now();
        let mut done = Vec::new();
        let mut sweep = Vec::new();
        for (&run_id, cr) in &self.runs {
            let closed = cr.board.as_ref().is_some_and(|b| b.is_closed());
            let finished = cr.ended && cr.waiting.is_empty() && cr.in_flight == 0;
            let stale = cr.started.elapsed() > Duration::from_secs(30);
            if closed || finished || stale {
                done.push((run_id, stale && !closed));
            } else if cr.board.is_some() && !cr.waiting.is_empty() {
                // Safety net for a missed wake-up (e.g. an agave-done event dropped).
                sweep.push(run_id);
            }
        }
        for (run_id, abandon) in done {
            self.drop_run(run_id, abandon);
        }
        for run_id in sweep {
            let ks: Vec<TxIdx> = self.runs[&run_id].waiting.keys().copied().collect();
            for k in ks {
                self.try_dispatch(run_id, k);
            }
        }
    }
}

/// Hand FL's result of a sampled transaction to agave's handler (and decline the cell).
fn deposit(board: &Board, index: usize, processed: FlProcessed, metrics: &CommitMetrics) {
    let FlProcessed {
        result,
        balances,
        recorded,
        ..
    } = processed;
    board.deposit_sample(
        index,
        SampleDeposit {
            result,
            balances: balances.and_then(|b| fast_lane_commit::tx_balances(b).into_iter().next()),
            recorded,
        },
    );
    metrics.deposits.fetch_add(1, Ordering::Relaxed);
}

/// Commit thread body: commit jobs through agave's commit path, report to the coordinator.
pub fn commit_worker_loop(
    jobs: Receiver<CommitJob>,
    coord_tx: Sender<CoordMsg>,
    services: ReplayServices,
    exit: Arc<AtomicBool>,
    spin: Duration,
    metrics: Arc<CommitMetrics>,
) {
    let services_ref = CommitServices {
        transaction_status_sender: services.transaction_status_sender.as_ref(),
        replay_vote_sender: services.replay_vote_sender.as_ref(),
        prioritization_fee_cache: services.prioritization_fee_cache.as_deref(),
    };
    while let Some(job) = recv_spin(&jobs, spin, &exit) {
        let CommitJob {
            run_id,
            k,
            index,
            bank,
            board,
            tx,
            processed,
            t_final,
            bytes,
        } = job;
        mem::COMMIT_PENDING_BYTES.sub(bytes);
        let t0 = Instant::now();
        let outcome = if !control::committing() {
            CommitOutcome::Stopped
        } else {
            let FlProcessed {
                result,
                balances,
                check,
                ..
            } = *processed;
            let mut timings = ExecuteTimings::default();
            match catch_unwind(AssertUnwindSafe(|| {
                commit_external(
                    &bank,
                    &board,
                    index,
                    &tx.rtx,
                    &check,
                    result,
                    balances,
                    services_ref,
                    &mut timings,
                )
            })) {
                Ok(ExternalCommit::Committed(result)) => CommitOutcome::Committed(result.is_ok()),
                Ok(ExternalCommit::Declined(class)) => CommitOutcome::Declined(class),
                Ok(ExternalCommit::Lost) => CommitOutcome::Lost,
                Err(_) => CommitOutcome::Failed,
            }
        };
        let t_done = Instant::now();
        CommitMetrics::sample(
            &metrics.service_us,
            t_done.saturating_duration_since(t0).as_micros() as u64,
        );
        drop((bank, board, tx));
        if coord_tx
            .send(CoordMsg::Sink(Box::new(CommitEvent::Done {
                run_id,
                k,
                outcome,
                t_final,
                t_done,
            })))
            .is_err()
        {
            break;
        }
    }
}

/// Percentile of a sample (sorted in place).
pub fn pct(v: &mut [u32], p: f64) -> u32 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// The interval's commit report (fast_lane_commit log line and datapoint).
pub struct CommitReport {
    prev: [u64; 20],
}

impl Default for CommitReport {
    fn default() -> Self {
        Self { prev: [0; 20] }
    }
}

impl CommitReport {
    pub fn report(&mut self, secs: f64, metrics: &CommitMetrics) -> String {
        let s = &fast_lane_commit::STATS;
        let now: [u64; 20] = [
            metrics.committed.load(Ordering::Relaxed),
            metrics.committed_err.load(Ordering::Relaxed),
            metrics.lost.load(Ordering::Relaxed),
            metrics.stopped.load(Ordering::Relaxed),
            metrics.failed.load(Ordering::Relaxed),
            metrics.runs_bound.load(Ordering::Relaxed),
            metrics.runs_unbound.load(Ordering::Relaxed),
            s.agave_bound.load(Ordering::Relaxed),
            s.agave_unbound.load(Ordering::Relaxed),
            s.follow_timeouts.load(Ordering::Relaxed),
            s.follow_waits.load(Ordering::Relaxed),
            s.follow_wait_us.load(Ordering::Relaxed),
            s.follow_done.load(Ordering::Relaxed),
            s.identity_mismatches.load(Ordering::Relaxed),
            s.replay_required.load(Ordering::Relaxed),
            s.samples.load(Ordering::Relaxed),
            s.sample_mismatches.load(Ordering::Relaxed),
            s.sample_timeouts.load(Ordering::Relaxed),
            s.unverified_commits.load(Ordering::Relaxed),
            s.agave_only_slots.load(Ordering::Relaxed),
        ];
        let d: Vec<u64> = now
            .iter()
            .zip(self.prev.iter())
            .map(|(a, b)| a.saturating_sub(*b))
            .collect();
        self.prev = now;
        let declined: Vec<(&str, u64)> = DECLINE_CLASSES
            .iter()
            .zip(&metrics.declined)
            .map(|(c, n)| (*c, n.swap(0, Ordering::Relaxed)))
            .filter(|(_, n)| *n > 0)
            .collect();
        let mut lat = metrics
            .latency_us
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default();
        let mut wait = metrics
            .bank_wait_us
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default();
        let mut service = metrics
            .service_us
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default();
        let max_waiting = metrics.max_waiting.swap(0, Ordering::Relaxed);
        let follow_wait_avg = if d[10] > 0 { d[11] / d[10] } else { 0 };
        let line = format!(
            "fast_lane_commit secs={secs:.1} mode={} committed={} committed_err={} \
             agave_bound={} agave_unbound={} declined={declined:?} lost={} stopped={} failed={} \
             commit_lat_us_p50={} p90={} p99={} commit_service_us_p50={} p99={} \
             bank_waits={} bank_wait_us_p50={} p90={} max={} runs_bound={} runs_unbound={} \
             follow_waits={} follow_wait_us_avg={follow_wait_avg} follow_timeouts={} \
             follow_done={} identity_mismatch={} replay_required={} samples={} \
             sample_mismatch={} sample_timeouts={} unverified={} agave_only_slots={} \
             max_waiting={max_waiting} mem_commit_kb={} mem_board_kb={}",
            control::commit_mode_name(control::commit_mode()),
            d[0],
            d[1],
            d[7],
            d[8],
            d[2],
            d[3],
            d[4],
            pct(&mut lat, 0.5),
            pct(&mut lat, 0.9),
            pct(&mut lat, 0.99),
            pct(&mut service, 0.5),
            pct(&mut service, 0.99),
            wait.len(),
            pct(&mut wait, 0.5),
            pct(&mut wait, 0.9),
            wait.iter().max().copied().unwrap_or(0),
            d[5],
            d[6],
            d[10],
            d[9],
            d[12],
            d[13],
            d[14],
            d[15],
            d[16],
            d[17],
            d[18],
            d[19],
            mem::COMMIT_PENDING_BYTES.get() >> 10,
            fast_lane_commit::BOARD_BYTES.load(Ordering::Relaxed) >> 10,
        );
        solana_metrics::datapoint_info!(
            "fast_lane_commit",
            ("committed", d[0] as i64, i64),
            ("agave_bound", d[7] as i64, i64),
            ("agave_unbound", d[8] as i64, i64),
            ("lost", d[2] as i64, i64),
            ("failed", d[4] as i64, i64),
            ("commit_lat_us_p50", i64::from(pct(&mut lat, 0.5)), i64),
            ("commit_lat_us_p90", i64::from(pct(&mut lat, 0.9)), i64),
            ("bank_wait_us_p90", i64::from(pct(&mut wait, 0.9)), i64),
            ("follow_timeouts", d[9] as i64, i64),
            ("identity_mismatch", d[13] as i64, i64),
            ("replay_required", d[14] as i64, i64),
            ("sample_mismatch", d[16] as i64, i64),
        );
        line
    }
}

/// Log once at startup.
pub fn log_start(config: &crate::config::Config) {
    info!(
        "fast lane: commit threads {} (cores {:?}), verify_sample_ppm {}, follow_wait_ms {}, \
         follow_spin_us {}",
        config.commit_threads,
        config.commit_cores,
        config.verify_sample_ppm,
        config.follow_wait_ms,
        config.follow_spin_us
    );
}

#[allow(dead_code)]
fn _slot(s: Slot) -> Slot {
    s
}
