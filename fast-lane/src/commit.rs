//! Execute-once milestone 2 (`commit = on`): FL commits its FINAL transactions into agave's
//! bank N through agave's own commit path (`solana_runtime::transaction_execution::
//! commit_external`), and agave's replay of N follows (`solana_runtime::fast_lane_commit`).
//!
//! The [`Committer`] runs on the coordinator thread (inside the FINAL sink). For each run it
//! binds to agave's bank of the run's slot once agave inserted it into bank forks (same
//! parent slot and bank id as the run), takes each FINAL transaction's full processing result
//! and hands it to a commit thread (or, with `commit_on = workers`, to FL's executor threads,
//! which are already spinning) as soon as the transaction's commit-order predecessors
//! (`Finalized::cpreds`: every earlier transaction it conflicts with the way agave's
//! scheduler orders them) are in the bank, committed by FL or by agave. Commits of
//! non-conflicting transactions run in parallel.
//!
//! FL declines (agave executes) what it must not commit: transactions touching a program
//! loader, results that modified programs, unprocessable results, results executed without
//! agave's recording configuration, and the verification sample (whose FL result is
//! deposited for agave's handler to compare before committing its own). With
//! `sample_mode = fl` FL picks the sample among transactions no later transaction conflicts
//! with, so a sample never delays another commit.
//!
//! Every committed transaction's FINAL → committed time is attributed (histograms in the
//! `fast_lane_commit` line, a sampled per-transaction `fl_commit.*.csv`): waiting for bank N,
//! waiting for commit-order predecessors by who owns the predecessor (FL's own commit, a
//! predecessor FL had not finalized yet, agave's execution of a sample / a declined
//! transaction / a transaction agave claimed first) and whether only the readers-since rule
//! made it a predecessor, queueing for a commit thread, and the commit itself.

use {
    crate::{
        ReplayServices,
        control,
        mem,
        mv::TxIdx,
        run::{FlProcessed, OutcomeKind, Run, TxEntry, TxOutcome},
        sched::{CoordMsg, RunId, RunSummary, SideJob, recv_spin},
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
    solana_signature::Signature,
    solana_svm::transaction_processing_result::ProcessedTransaction,
    solana_svm_timings::ExecuteTimings,
    std::{
        collections::{HashMap, HashSet, VecDeque},
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
    /// A commit job finished.
    Done {
        run_id: RunId,
        k: TxIdx,
        outcome: CommitOutcome,
        timing: Box<CommitTiming>,
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

/// Why a commit waited for a predecessor: who owns (commits) that predecessor.
pub const WAIT_CLASSES: [&str; 6] = [
    // FL commits it (queued behind its own predecessors, or its commit in progress).
    "fl_commit",
    // FL had not finalized (or decided) it yet.
    "fl_unfinal",
    // Agave executes it: the verification sample.
    "agave_sample",
    // Agave executes it: FL declined it (loader, check, lost claim, ...).
    "agave_declined",
    // Agave executes it: agave claimed it before FL (unbound bank, follow timeout).
    "agave_first",
    // Agave executes it for another reason.
    "agave_other",
];
const W_FL: usize = 0;
const W_UNFINAL: usize = 1;
const W_SAMPLE: usize = 2;
const W_DECLINED: usize = 3;
const W_FIRST: usize = 4;

/// Where one transaction's FINAL → committed time went.
#[derive(Clone, Debug, Default)]
pub struct CommitTiming {
    pub slot: Slot,
    pub ordinal: u32,
    pub signature: Signature,
    pub is_vote: bool,
    pub t_final: Option<Instant>,
    /// Waiting for agave to create bank N (FL bound the run after this FINAL).
    pub bank_wait: Duration,
    /// Waiting for commit-order predecessors, by [`WAIT_CLASSES`].
    pub wait: [Duration; 6],
    /// Part of `wait` on predecessors only the readers-since rule imposed.
    pub wait_readers: Duration,
    /// Class of the predecessor waited on longest (index into [`WAIT_CLASSES`]).
    pub blocker: Option<usize>,
    pub n_cpreds: u32,
    /// Handed to a commit thread / executor.
    pub t_dispatch: Option<Instant>,
    /// The job started on a thread.
    pub t_start: Option<Instant>,
    pub t_end: Option<Instant>,
    pub t_end_unix_ns: u64,
    pub on_worker: bool,
}

impl CommitTiming {
    pub fn queue(&self) -> Duration {
        match (self.t_dispatch, self.t_start) {
            (Some(a), Some(b)) => b.saturating_duration_since(a),
            _ => Duration::ZERO,
        }
    }

    pub fn service(&self) -> Duration {
        match (self.t_start, self.t_end) {
            (Some(a), Some(b)) => b.saturating_duration_since(a),
            _ => Duration::ZERO,
        }
    }

    pub fn total(&self) -> Duration {
        match (self.t_final, self.t_end) {
            (Some(a), Some(b)) => b.saturating_duration_since(a),
            _ => Duration::ZERO,
        }
    }

    pub fn pred_wait(&self) -> Duration {
        self.wait.iter().sum()
    }

    pub fn csv_header() -> &'static str {
        "slot,ordinal,signature,vote,commit_unix_ns,total_us,bank_wait_us,pred_wait_us,\
         wait_fl_commit_us,wait_fl_unfinal_us,wait_agave_sample_us,wait_agave_declined_us,\
         wait_agave_first_us,wait_agave_other_us,wait_readers_us,blocker,n_cpreds,queue_us,\
         service_us,on_worker"
    }

    pub fn csv_line(&self) -> String {
        let us = |d: Duration| d.as_micros();
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            self.slot,
            self.ordinal,
            self.signature,
            u8::from(self.is_vote),
            self.t_end_unix_ns,
            us(self.total()),
            us(self.bank_wait),
            us(self.pred_wait()),
            us(self.wait[0]),
            us(self.wait[1]),
            us(self.wait[2]),
            us(self.wait[3]),
            us(self.wait[4]),
            us(self.wait[5]),
            us(self.wait_readers),
            self.blocker.map(|b| WAIT_CLASSES[b]).unwrap_or(""),
            self.n_cpreds,
            us(self.queue()),
            us(self.service()),
            u8::from(self.on_worker),
        )
    }
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
    pub bytes: i64,
    pub timing: Box<CommitTiming>,
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
    /// Samples FL picked (`sample_mode = fl`) and sample credit left unused at the end of runs.
    pub samples_picked: AtomicU64,
    /// Committed transactions whose longest predecessor wait was of each class.
    pub blockers: [AtomicU64; 6],
    /// Commits run on FL's executor threads.
    pub on_workers: AtomicU64,
    /// µs samples of the interval: FINAL → committed, bank wait of a run's first FINAL,
    /// commit service, queue (dispatch → start), predecessor wait (total, on agave, on
    /// readers-since predecessors), bank wait per transaction.
    pub latency_us: Mutex<Vec<u32>>,
    pub bank_wait_us: Mutex<Vec<u32>>,
    pub service_us: Mutex<Vec<u32>>,
    pub queue_us: Mutex<Vec<u32>>,
    pub pred_wait_us: Mutex<Vec<u32>>,
    pub pred_wait_agave_us: Mutex<Vec<u32>>,
    pub pred_wait_readers_us: Mutex<Vec<u32>>,
    pub tx_bank_wait_us: Mutex<Vec<u32>>,
    /// Summed µs of predecessor waits per class over the interval.
    pub wait_sum_us: [AtomicU64; 6],
    pub wait_readers_sum_us: AtomicU64,
    pub bank_wait_sum_us: AtomicU64,
    pub queue_sum_us: AtomicU64,
    pub service_sum_us: AtomicU64,
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

    /// Account one committed transaction's timing.
    fn record(&self, t: &CommitTiming) {
        let us = |d: Duration| d.as_micros() as u64;
        Self::sample(&self.latency_us, us(t.total()));
        Self::sample(&self.queue_us, us(t.queue()));
        Self::sample(&self.pred_wait_us, us(t.pred_wait()));
        Self::sample(&self.pred_wait_agave_us, us(t.wait[W_SAMPLE..].iter().sum()));
        Self::sample(&self.pred_wait_readers_us, us(t.wait_readers));
        Self::sample(&self.tx_bank_wait_us, us(t.bank_wait));
        for (sum, w) in self.wait_sum_us.iter().zip(&t.wait) {
            sum.fetch_add(us(*w), Ordering::Relaxed);
        }
        self.wait_readers_sum_us
            .fetch_add(us(t.wait_readers), Ordering::Relaxed);
        self.bank_wait_sum_us
            .fetch_add(us(t.bank_wait), Ordering::Relaxed);
        self.queue_sum_us.fetch_add(us(t.queue()), Ordering::Relaxed);
        self.service_sum_us
            .fetch_add(us(t.service()), Ordering::Relaxed);
        if let Some(b) = t.blocker {
            self.blockers[b].fetch_add(1, Ordering::Relaxed);
        }
        if t.on_worker {
            self.on_workers.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct Pending {
    cpreds: Vec<TxIdx>,
    cpred_writers: usize,
    tx: Arc<TxEntry>,
    processed: Box<FlProcessed>,
    bytes: i64,
    /// FL picked it for the verification sample (`sample_mode = fl`).
    sample: bool,
    /// The predecessor it is registered as waiting for (in `CommitRun::waiters`), since when,
    /// its wait class, and whether only the readers-since rule made it a predecessor.
    parked_on: Option<TxIdx>,
    park: Option<(Instant, usize, bool)>,
    /// Longest single predecessor wait so far (for `blocker`).
    longest: Duration,
    timing: Box<CommitTiming>,
}

impl Pending {
    /// End the current predecessor wait (if any) at `now`.
    fn unpark(&mut self, now: Instant) {
        if let Some((since, class, reader)) = self.park.take() {
            let d = now.saturating_duration_since(since);
            self.timing.wait[class] += d;
            if reader {
                self.timing.wait_readers += d;
            }
            if d >= self.longest {
                self.longest = d;
                self.timing.blocker = Some(class);
            }
        }
    }
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
    declines: Vec<(TxIdx, Option<Box<FlProcessed>>, bool)>,
    /// Dispatched, not done.
    inflight: HashSet<TxIdx>,
    /// Transactions agave executes, with the reason (a decline class).
    agave: HashMap<TxIdx, &'static str>,
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

    /// Who owns predecessor `j` of a waiting transaction.
    fn classify(&self, board: &Board, j: TxIdx) -> usize {
        if self.inflight.contains(&j) || self.waiting.contains_key(&j) {
            return W_FL;
        }
        if let Some(class) = self.agave.get(&j) {
            return if *class == "sample" { W_SAMPLE } else { W_DECLINED };
        }
        let index = (j - self.base) as usize;
        match board.state_of(index) {
            fast_lane_commit::FL_CLAIMED => W_FL,
            fast_lane_commit::FL_DECLINED => W_DECLINED,
            fast_lane_commit::AGAVE_CLAIMED => {
                let hash_sample = board.sample_mode == fast_lane_commit::SAMPLE_HASH
                    && self.run.tx(j).is_some_and(|tx| {
                        fast_lane_commit::sampled(&tx.signature, board.sample_ppm)
                    });
                if hash_sample { W_SAMPLE } else { W_FIRST }
            }
            _ => W_UNFINAL,
        }
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
    /// `commit_on = workers`: FL's executor threads' side-job channel, with what a job needs.
    workers: Option<WorkerCommit>,
    /// Sampled per-transaction timings for the `fl_commit` CSV (sent to the comparator).
    records: Option<Sender<crate::compare::CmpMsg>>,
    record_credit: u64,
    /// Sample credit (parts per million accumulated per FINAL transaction), per vote /
    /// non-vote so that both are sampled.
    sample_credit: [u64; 2],
}

struct WorkerCommit {
    side_tx: Sender<SideJob>,
    coord_tx: Sender<CoordMsg>,
    services: Arc<ReplayServices>,
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
            workers: None,
            records: None,
            record_credit: 0,
            sample_credit: [0; 2],
        }
    }

    /// Also commit on FL's executor threads when `commit_on = workers`.
    pub fn with_workers(
        mut self,
        side_tx: Sender<SideJob>,
        coord_tx: Sender<CoordMsg>,
        services: ReplayServices,
    ) -> Self {
        self.workers = Some(WorkerCommit {
            side_tx,
            coord_tx,
            services: Arc::new(services),
        });
        self
    }

    /// Send sampled per-transaction timings to the comparator (`fl_commit` CSV).
    pub fn with_records(mut self, records: Sender<crate::compare::CmpMsg>) -> Self {
        self.records = Some(records);
        self
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
                timing,
            } => {
                let m = &self.metrics;
                match &outcome {
                    CommitOutcome::Committed(ok) => {
                        m.committed.fetch_add(1, Ordering::Relaxed);
                        if !ok {
                            m.committed_err.fetch_add(1, Ordering::Relaxed);
                        }
                        m.record(&timing);
                        self.maybe_record(timing);
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
                    run.inflight.remove(&k);
                    match outcome {
                        CommitOutcome::Declined(class) => {
                            run.agave.insert(k, class);
                        }
                        CommitOutcome::Lost => {
                            run.agave.insert(k, "lost");
                        }
                        _ => {}
                    }
                }
                self.wake(run_id, k);
            }
        }
    }

    fn maybe_record(&mut self, timing: Box<CommitTiming>) {
        let Some(records) = &self.records else {
            return;
        };
        self.record_credit += u64::from(control::commit_csv_ppm());
        if self.record_credit < 1_000_000 {
            return;
        }
        self.record_credit -= 1_000_000;
        let _ = records.try_send(crate::compare::CmpMsg::Commit(timing));
    }

    fn on_run_begin(&mut self, run_id: RunId, run: Arc<Run>) {
        if !control::committing() {
            return;
        }
        let base = run.ordinal_base;
        fast_lane_commit::expect_fl_run(run.slot, run.parent_bank_id);
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
                inflight: HashSet::new(),
                agave: HashMap::new(),
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
        for (k, processed, pick) in std::mem::take(&mut cr.declines) {
            let index = (k - cr.base) as usize;
            if let Some(processed) = processed {
                mem::COMMIT_PENDING_BYTES.sub(pending_bytes(&processed));
                let hash_sample = board.sample_mode == fast_lane_commit::SAMPLE_HASH
                    && cr
                        .run
                        .tx(k)
                        .is_some_and(|tx| fast_lane_commit::sampled(&tx.signature, board.sample_ppm));
                if hash_sample || (pick && board.sample_mode == fast_lane_commit::SAMPLE_FL) {
                    deposit(&board, index, *processed, &self.metrics);
                    continue;
                }
            }
            board.decline(index);
        }
        if let Some(total) = cr.total {
            board.set_fl_total(total);
        }
        // Transactions FINAL before the binding waited for bank N.
        for pending in cr.waiting.values_mut() {
            if let Some(t_final) = pending.timing.t_final {
                pending.timing.bank_wait = now.saturating_duration_since(t_final);
            }
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
    #[allow(clippy::too_many_arguments)]
    pub fn on_final(
        &mut self,
        run_id: RunId,
        k: TxIdx,
        cpreds: &[TxIdx],
        cpred_writers: usize,
        isolated: bool,
        outcome: &mut TxOutcome,
        t_final: Instant,
    ) {
        if !control::committing() {
            return;
        }
        let Some(base) = self.runs.get(&run_id).map(|cr| cr.base) else {
            return;
        };
        if k < base {
            return;
        }
        let Some(cr) = self.runs.get_mut(&run_id) else {
            return;
        };
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
            cr.agave.insert(k, class);
            let index = (k - cr.base) as usize;
            match &cr.board {
                Some(board) => {
                    // A sampled transaction gets FL's result deposited whatever the reason
                    // FL does not commit it (agave's handler compares it).
                    let hash_sample = board.sample_mode == fast_lane_commit::SAMPLE_HASH
                        && fast_lane_commit::sampled(&tx.signature, board.sample_ppm);
                    match outcome.processed.take() {
                        Some(processed) if hash_sample => {
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
                    cr.declines.push((k, processed, false));
                }
            }
            return;
        }
        let Some(processed) = outcome.processed.take() else {
            return;
        };
        let pick = pick_sample(&mut self.sample_credit, &self.metrics, outcome.is_vote, isolated);
        let bytes = pending_bytes(&processed);
        mem::COMMIT_PENDING_BYTES.add(bytes);
        cr.waiting.insert(
            k,
            Pending {
                cpreds: cpreds.to_vec(),
                cpred_writers,
                tx: Arc::clone(&tx),
                processed,
                bytes,
                sample: pick,
                parked_on: None,
                park: None,
                longest: Duration::ZERO,
                timing: Box::new(CommitTiming {
                    slot: outcome.slot,
                    ordinal: outcome.ordinal,
                    signature: outcome.signature,
                    is_vote: outcome.is_vote,
                    t_final: Some(t_final),
                    n_cpreds: cpreds.len() as u32,
                    ..CommitTiming::default()
                }),
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
        let sample = match board.sample_mode {
            fast_lane_commit::SAMPLE_HASH => {
                fast_lane_commit::sampled(&pending.tx.signature, board.sample_ppm)
            }
            _ => pending.sample,
        };
        if sample {
            // Agave executes it and compares with this result before committing its own.
            let pending = cr.waiting.remove(&k).unwrap();
            mem::COMMIT_PENDING_BYTES.sub(pending.bytes);
            cr.agave.insert(k, "sample");
            self.metrics.decline("sample");
            deposit(&board, index, *pending.processed, &self.metrics);
            return;
        }
        let missing = pending
            .cpreds
            .iter()
            .enumerate()
            .find(|&(_, &j)| j >= base && !board.is_done((j - base) as usize))
            .map(|(i, &j)| (j, i >= pending.cpred_writers));
        let now = Instant::now();
        if let Some((missing, reader)) = missing {
            if pending.parked_on != Some(missing) {
                let class = cr.classify(&board, missing);
                cr.waiters.entry(missing).or_default().push(k);
                let pending = cr.waiting.get_mut(&k).unwrap();
                pending.unpark(now);
                pending.parked_on = Some(missing);
                pending.park = Some((now, class, reader));
            }
            return;
        }
        let mut pending = cr.waiting.remove(&k).unwrap();
        pending.unpark(now);
        cr.inflight.insert(k);
        let mut timing = pending.timing;
        timing.t_dispatch = Some(now);
        let job = CommitJob {
            run_id,
            k,
            index,
            bank,
            board,
            tx: pending.tx,
            processed: pending.processed,
            bytes: pending.bytes,
            timing,
        };
        match &self.workers {
            Some(w) if control::commit_on_workers() => {
                let (coord_tx, services, metrics) = (
                    w.coord_tx.clone(),
                    Arc::clone(&w.services),
                    Arc::clone(&self.metrics),
                );
                let side: SideJob = Box::new(move || {
                    run_commit_job(job, &services, &metrics, &coord_tx, true);
                });
                if w.side_tx.send(side).is_err() {
                    warn!("fast lane: executor threads are gone");
                }
            }
            _ => {
                if self.job_tx.send(job).is_err() {
                    warn!("fast lane: commit threads are gone");
                }
            }
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
            .filter_map(|(_, p, _)| p.as_deref().map(pending_bytes))
            .sum();
        mem::COMMIT_PENDING_BYTES.sub(decline_bytes);
        // The committer no longer binds this run: nobody should wait for it (and a bank of
        // the slot it never bound is agave's alone).
        if cr.board.is_some() {
            fast_lane_commit::unexpect_fl_run(cr.run.slot);
        } else {
            fast_lane_commit::fl_skips_slot(cr.run.slot);
        }
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
            let finished = cr.ended && cr.waiting.is_empty() && cr.inflight.is_empty();
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

/// Whether FL picks this committable FINAL transaction for the verification sample
/// (`sample_mode = fl`): the sample rate accrues as credit (per vote / non-vote), spent on the
/// next transaction no later transaction conflicts with, so the sample, which agave executes
/// at its position, never delays a later commit.
fn pick_sample(credits: &mut [u64; 2], metrics: &CommitMetrics, is_vote: bool, isolated: bool) -> bool {
    if fast_lane_commit::sample_mode() != fast_lane_commit::SAMPLE_FL {
        return false;
    }
    let credit = &mut credits[usize::from(is_vote)];
    *credit = (*credit + u64::from(fast_lane_commit::sample_ppm())).min(4_000_000);
    if *credit >= 1_000_000 && isolated {
        *credit -= 1_000_000;
        metrics.samples_picked.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
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

/// Commit one job through agave's commit path and report it to the coordinator. Runs on a
/// commit thread or on an executor thread (`on_worker`). Returns false if the coordinator is
/// gone.
fn run_commit_job(
    job: CommitJob,
    services: &ReplayServices,
    metrics: &CommitMetrics,
    coord_tx: &Sender<CoordMsg>,
    on_worker: bool,
) -> bool {
    let CommitJob {
        run_id,
        k,
        index,
        bank,
        board,
        tx,
        processed,
        bytes,
        mut timing,
    } = job;
    mem::COMMIT_PENDING_BYTES.sub(bytes);
    let t0 = Instant::now();
    timing.t_start = Some(t0);
    timing.on_worker = on_worker;
    let outcome = if !control::committing() {
        CommitOutcome::Stopped
    } else {
        let FlProcessed {
            result,
            balances,
            check,
            ..
        } = *processed;
        let services_ref = CommitServices {
            transaction_status_sender: services.transaction_status_sender.as_ref(),
            replay_vote_sender: services.replay_vote_sender.as_ref(),
            prioritization_fee_cache: services.prioritization_fee_cache.as_deref(),
        };
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
    let t_end = Instant::now();
    timing.t_end = Some(t_end);
    timing.t_end_unix_ns = crate::tap::unix_ns();
    CommitMetrics::sample(
        &metrics.service_us,
        t_end.saturating_duration_since(t0).as_micros() as u64,
    );
    drop((bank, board, tx));
    coord_tx
        .send(CoordMsg::Sink(Box::new(CommitEvent::Done {
            run_id,
            k,
            outcome,
            timing,
        })))
        .is_ok()
}

/// Commit thread body: commit jobs through agave's commit path, report to the coordinator.
/// The busy-poll time before parking is `control::commit_spin_us` (runtime), seeded with
/// `spin`.
pub fn commit_worker_loop(
    jobs: Receiver<CommitJob>,
    coord_tx: Sender<CoordMsg>,
    services: ReplayServices,
    exit: Arc<AtomicBool>,
    spin: Duration,
    metrics: Arc<CommitMetrics>,
) {
    if control::commit_spin_us() == u64::MAX {
        control::set_commit_spin_us(spin.as_micros() as u64);
    }
    loop {
        let spin = Duration::from_micros(control::commit_spin_us());
        let Some(job) = recv_spin(&jobs, spin, &exit) else {
            return;
        };
        if !run_commit_job(job, &services, &metrics, &coord_tx, false) {
            return;
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

fn take(v: &Mutex<Vec<u32>>) -> Vec<u32> {
    v.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
}

/// The interval's commit report (fast_lane_commit log line and datapoint).
pub struct CommitReport {
    prev: [u64; 22],
    prev_wait_txs: [u64; 3],
    prev_unbound: [u64; 10],
}

impl Default for CommitReport {
    fn default() -> Self {
        Self {
            prev: [0; 22],
            prev_wait_txs: [0; 3],
            prev_unbound: [0; 10],
        }
    }
}

/// Deltas of a cumulative counter array since `prev` (updated), as non-zero (name, n) pairs.
fn named_deltas<const N: usize>(
    names: &[&'static str; N],
    now: &[AtomicU64; N],
    prev: &mut [u64; N],
) -> Vec<(&'static str, u64)> {
    let mut out = Vec::new();
    for i in 0..N {
        let v = now[i].load(Ordering::Relaxed);
        let d = v.saturating_sub(prev[i]);
        prev[i] = v;
        if d > 0 {
            out.push((names[i], d));
        }
    }
    out
}

fn unbound_class(classes: &[(&'static str, u64)], name: &str) -> i64 {
    classes
        .iter()
        .find(|(n, _)| *n == name)
        .map_or(0, |(_, c)| *c as i64)
}

/// The interval's per-bank binding waits: count, outcomes and µs percentiles per reason.
struct BindWaitReport {
    boards: Vec<(&'static str, u64)>,
    outcomes: Vec<(String, u64)>,
    /// (reason, p50, p90, max) µs.
    us: Vec<(&'static str, u32, u32, u32)>,
    known: (u64, u32, u32, u32, u64),
    parent: (u64, u32, u32, u32, u64),
}

impl BindWaitReport {
    fn new(waits: Vec<fast_lane_commit::BindWait>) -> Self {
        let reasons = fast_lane_commit::BIND_WAIT_REASONS;
        let outcomes_names = fast_lane_commit::BIND_WAIT_OUTCOMES;
        let mut boards = Vec::new();
        let mut outcomes = Vec::new();
        let mut us = Vec::new();
        let per = |reason: usize| {
            let mut v: Vec<u32> = waits
                .iter()
                .filter(|w| w.reason as usize == reason)
                .map(|w| w.us)
                .collect();
            let timeouts = waits
                .iter()
                .filter(|w| w.reason as usize == reason && w.outcome == 1)
                .count() as u64;
            let n = v.len() as u64;
            let max = v.iter().max().copied().unwrap_or(0);
            (n, pct(&mut v, 0.5), pct(&mut v, 0.9), max, timeouts)
        };
        let known = per(1);
        let parent = per(2);
        for (i, name) in reasons.iter().enumerate() {
            let (n, p50, p90, max, _) = per(i);
            if n > 0 {
                boards.push((*name, n));
                us.push((*name, p50, p90, max));
            }
            for (o, oname) in outcomes_names.iter().enumerate() {
                let c = waits
                    .iter()
                    .filter(|w| w.reason as usize == i && w.outcome as usize == o)
                    .count() as u64;
                if c > 0 {
                    outcomes.push((format!("{name}:{oname}"), c));
                }
            }
        }
        Self {
            boards,
            outcomes,
            us,
            known,
            parent,
        }
    }
}

impl CommitReport {
    pub fn report(&mut self, secs: f64, metrics: &CommitMetrics) -> String {
        let s = &fast_lane_commit::STATS;
        let now: [u64; 22] = [
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
            s.bind_waits.load(Ordering::Relaxed),
            s.bind_wait_timeouts.load(Ordering::Relaxed),
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
        let blockers: Vec<(&str, u64)> = WAIT_CLASSES
            .iter()
            .zip(&metrics.blockers)
            .map(|(c, n)| (*c, n.swap(0, Ordering::Relaxed)))
            .filter(|(_, n)| *n > 0)
            .collect();
        // Where the interval's FINAL → committed time went (ms summed over transactions).
        let ms = |a: &AtomicU64| a.swap(0, Ordering::Relaxed) / 1000;
        let wait_ms: Vec<(&str, u64)> = WAIT_CLASSES
            .iter()
            .zip(&metrics.wait_sum_us)
            .map(|(c, n)| (*c, ms(n)))
            .filter(|(_, n)| *n > 0)
            .collect();
        let readers_ms = ms(&metrics.wait_readers_sum_us);
        let bank_ms = ms(&metrics.bank_wait_sum_us);
        let queue_ms = ms(&metrics.queue_sum_us);
        let service_ms = ms(&metrics.service_sum_us);
        let mut lat = take(&metrics.latency_us);
        let mut wait = take(&metrics.bank_wait_us);
        let mut service = take(&metrics.service_us);
        let mut queue = take(&metrics.queue_us);
        let mut pred = take(&metrics.pred_wait_us);
        let mut pred_agave = take(&metrics.pred_wait_agave_us);
        let mut readers = take(&metrics.pred_wait_readers_us);
        let mut tx_bank = take(&metrics.tx_bank_wait_us);
        let max_waiting = metrics.max_waiting.swap(0, Ordering::Relaxed);
        let picked = metrics.samples_picked.swap(0, Ordering::Relaxed);
        let on_workers = metrics.on_workers.swap(0, Ordering::Relaxed);
        let follow_wait_avg = if d[10] > 0 { d[11] / d[10] } else { 0 };
        let wait_txs = named_deltas(
            &fast_lane_commit::BIND_WAIT_REASONS,
            &s.bind_wait_txs,
            &mut self.prev_wait_txs,
        );
        let unbound_why = named_deltas(
            &fast_lane_commit::UNBOUND_CLASSES,
            &s.unbound_why,
            &mut self.prev_unbound,
        );
        let bw = BindWaitReport::new(fast_lane_commit::take_bind_waits());
        let line = format!(
            "fast_lane_commit secs={secs:.1} mode={} on={} sample_mode={} committed={} \
             committed_err={} on_workers={on_workers} agave_bound={} agave_unbound={} \
             declined={declined:?} lost={} stopped={} failed={} commit_lat_us_p50={} p75={} \
             p90={} p99={} pred_wait_us_p50={} p75={} p90={} p99={} pred_agave_us_p90={} p99={} \
             readers_us_p90={} p99={} tx_bank_wait_us_p90={} p99={} queue_us_p50={} p90={} \
             p99={} commit_service_us_p50={} p90={} p99={} blocker={blockers:?} \
             wait_ms={wait_ms:?} readers_ms={readers_ms} bank_ms={bank_ms} queue_ms={queue_ms} \
             service_ms={service_ms} bank_waits={} bank_wait_us_p50={} p90={} max={} \
             bind_waits={} bind_wait_timeouts={} bind_wait_on={} bind_wait_txs={wait_txs:?} \
             bind_wait_banks={:?} bind_wait_outcomes={:?} bind_wait_us={:?} \
             agave_unbound_why={unbound_why:?} runs_bound={} runs_unbound={} follow_waits={} \
             follow_wait_us_avg={follow_wait_avg} follow_timeouts={} follow_done={} \
             identity_mismatch={} replay_required={} samples={} samples_picked={picked} \
             sample_mismatch={} sample_timeouts={} unverified={} agave_only_slots={} \
             max_waiting={max_waiting} mem_commit_kb={} mem_board_kb={}",
            control::commit_mode_name(control::commit_mode()),
            if control::commit_on_workers() { "workers" } else { "threads" },
            if fast_lane_commit::sample_mode() == fast_lane_commit::SAMPLE_FL { "fl" } else { "hash" },
            d[0],
            d[1],
            d[7],
            d[8],
            d[2],
            d[3],
            d[4],
            pct(&mut lat, 0.5),
            pct(&mut lat, 0.75),
            pct(&mut lat, 0.9),
            pct(&mut lat, 0.99),
            pct(&mut pred, 0.5),
            pct(&mut pred, 0.75),
            pct(&mut pred, 0.9),
            pct(&mut pred, 0.99),
            pct(&mut pred_agave, 0.9),
            pct(&mut pred_agave, 0.99),
            pct(&mut readers, 0.9),
            pct(&mut readers, 0.99),
            pct(&mut tx_bank, 0.9),
            pct(&mut tx_bank, 0.99),
            pct(&mut queue, 0.5),
            pct(&mut queue, 0.9),
            pct(&mut queue, 0.99),
            pct(&mut service, 0.5),
            pct(&mut service, 0.9),
            pct(&mut service, 0.99),
            wait.len(),
            pct(&mut wait, 0.5),
            pct(&mut wait, 0.9),
            wait.iter().max().copied().unwrap_or(0),
            d[20],
            d[21],
            crate::config::bind_wait_on_name(fast_lane_commit::bind_on()),
            bw.boards,
            bw.outcomes,
            bw.us,
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
            ("commit_lat_us_p99", i64::from(pct(&mut lat, 0.99)), i64),
            ("pred_wait_us_p90", i64::from(pct(&mut pred, 0.9)), i64),
            ("queue_us_p90", i64::from(pct(&mut queue, 0.9)), i64),
            ("bank_wait_us_p90", i64::from(pct(&mut wait, 0.9)), i64),
            ("bind_wait_timeouts", d[21] as i64, i64),
            ("known_waits", bw.known.0 as i64, i64),
            ("known_wait_us_p50", i64::from(bw.known.1), i64),
            ("known_wait_us_p90", i64::from(bw.known.2), i64),
            ("known_wait_us_max", i64::from(bw.known.3), i64),
            ("known_wait_timeouts", bw.known.4 as i64, i64),
            ("parent_waits", bw.parent.0 as i64, i64),
            ("parent_wait_us_p90", i64::from(bw.parent.2), i64),
            ("parent_wait_timeouts", bw.parent.4 as i64, i64),
            ("unbound_unknown", unbound_class(&unbound_why, "unknown"), i64),
            ("unbound_timeout", unbound_class(&unbound_why, "timeout"), i64),
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
        "fast lane: commit threads {} (cores {:?}), commit_on {}, verify_sample_ppm {}, \
         sample_mode {}, follow_wait_ms {}, follow_spin_us {}, bind_wait_us {}, bind_wait_on {}",
        config.commit_threads,
        config.commit_cores,
        if config.commit_on_workers { "workers" } else { "threads" },
        config.verify_sample_ppm,
        if config.sample_mode == fast_lane_commit::SAMPLE_FL { "fl" } else { "hash" },
        config.follow_wait_ms,
        config.follow_spin_us,
        config.bind_wait_us,
        crate::config::bind_wait_on_name(config.bind_wait_on),
    );
}
