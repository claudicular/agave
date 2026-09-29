//! Shadow comparator (`solFlCmp`): joins every FINAL fast-lane frame with agave's grouped
//! notification for the same (slot, signature), classifies exactness, measures lead time,
//! runs the slot-level check at freeze, writes the per-transaction export and logs interval
//! summaries.
//!
//! Clocks: lead time uses `Instant` (monotonic) on both sides; the export also carries
//! CLOCK_REALTIME nanoseconds (`*_unix_ns`) so external tools can join against other
//! wall-clock stamps on the same host (e.g. geyserbench's shmem arrival times).

use {
    crate::{
        config::Config,
        export::RotatingWriter,
        full_cmp::{FlFull, FullCompare},
        held::Held,
        mv::{accounts_equal, same_value},
        run::{OutcomeKind, Run, TxOutcome},
        sched::{FinalSink, Finalized, RunId, RunSummary},
        tap::unix_ns,
        tees::AgaveFrame,
    },
    crossbeam_channel::{Receiver, Sender, TrySendError, select},
    log::{info, warn},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::{BankId, Slot},
    solana_pubkey::Pubkey,
    solana_runtime::{bank_forks::BankForks, fast_lane_commit::AgaveProcessed},
    solana_sdk_ids::incinerator,
    solana_signature::Signature,
    std::{
        collections::HashMap,
        fmt::Write as _,
        path::PathBuf,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    },
};

const TOKEN_PROGRAM: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_2022_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

pub struct FinalRecord {
    pub run_id: RunId,
    pub outcome: TxOutcome,
    pub incarnations: u32,
    pub speculative: bool,
    pub n_preds: usize,
    pub t_ingest: Instant,
    pub t_first_dispatch: Instant,
    pub t_ready: Instant,
    pub rebased: u32,
    pub t_exec_start: Instant,
    pub t_exec_end: Instant,
    pub t_final: Instant,
    pub t_final_unix_ns: u64,
}

pub enum CmpMsg {
    Final(Box<FinalRecord>),
    RunStart {
        run_id: RunId,
        run: Arc<Run>,
        parent_frozen_at_first_set: bool,
        parent_wait_us: u64,
        ctx_build_us: u64,
    },
    RunEnd {
        run_id: RunId,
        summary: RunSummary,
    },
    SlotSkipped {
        slot: Slot,
        reason: &'static str,
    },
    SlotFrozen {
        slot: Slot,
        bank_id: BankId,
        t: Instant,
    },
    Rooted {
        slot: Slot,
    },
    /// A committed transaction's FINAL → committed attribution (sampled, `commit_csv_ppm`).
    Commit(Box<crate::commit::CommitTiming>),
}

/// The coordinator's sink: commits FINAL transactions into agave's banks (commit mode),
/// publishes them into the output ring (if enabled) and forwards them to the comparator.
pub struct CmpSink {
    pub tx: Sender<CmpMsg>,
    pub drops: Arc<AtomicU64>,
    pub out: Option<crate::output::OutPublisher>,
    pub committer: Option<crate::commit::Committer>,
}

impl FinalSink for CmpSink {
    fn on_final(&mut self, f: Finalized) {
        let Ok(mut outcome) = f.payload.downcast::<TxOutcome>() else {
            return;
        };
        if let Some(committer) = self.committer.as_mut() {
            committer.on_final(
                f.run_id,
                f.k,
                &f.cpreds,
                f.cpred_writers,
                f.isolated,
                &mut outcome,
                f.t_final,
            );
        }
        if let Some(out) = self.out.as_mut() {
            out.on_final(f.run_id, &outcome, f.incarnations, f.speculative);
        }
        let record = FinalRecord {
            run_id: f.run_id,
            outcome: *outcome,
            incarnations: f.incarnations,
            speculative: f.speculative,
            n_preds: f.n_preds,
            t_ingest: f.t_ingest,
            t_first_dispatch: f.t_first_dispatch,
            t_ready: f.t_ready,
            rebased: f.rebased,
            t_exec_start: f.t_exec_start,
            t_exec_end: f.t_exec_end,
            t_final: f.t_final,
            t_final_unix_ns: unix_ns(),
        };
        let bytes = record_bytes(&record);
        crate::mem::CMP_QUEUE_BYTES.add(bytes);
        if let Err(err) = self.tx.try_send(CmpMsg::Final(Box::new(record))) {
            crate::mem::CMP_QUEUE_BYTES.sub(bytes);
            if matches!(err, TrySendError::Full(_)) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn on_run_end(&mut self, run_id: RunId, summary: RunSummary) {
        if let Some(committer) = self.committer.as_mut() {
            committer.on_run_end(run_id, &summary);
        }
        if let Some(out) = self.out.as_mut() {
            out.on_run_end(run_id, &summary);
        }
        let _ = self.tx.try_send(CmpMsg::RunEnd { run_id, summary });
    }

    fn on_abort_ended(&mut self, run_id: RunId, reason: &'static str) {
        if let Some(out) = self.out.as_mut() {
            out.on_abort_ended(run_id, reason);
        }
    }

    fn tick(&mut self) {
        if let Some(committer) = self.committer.as_mut() {
            committer.tick();
        }
        if let Some(out) = self.out.as_mut() {
            out.tick();
        }
    }

    fn on_event(&mut self, event: Box<dyn std::any::Any + Send>) {
        if let (Some(committer), Ok(event)) = (
            self.committer.as_mut(),
            event.downcast::<crate::commit::CommitEvent>(),
        ) {
            committer.on_event(*event);
        }
    }
}

/// Bytes an agave frame holds (for the memory gauges).
pub fn agave_frame_bytes(frame: &AgaveFrame) -> i64 {
    crate::mem::frame_bytes(frame.accounts.iter().map(|(_, a)| a)) + 256
}

/// Bytes a FINAL record holds (its outcome's frame and full processing result).
pub fn record_bytes(record: &FinalRecord) -> i64 {
    record
        .outcome
        .frame
        .as_ref()
        .map(|f| crate::mem::frame_bytes(f.iter().map(|(_, a)| a)))
        .unwrap_or(0)
        + record
            .outcome
            .processed
            .as_ref()
            .map(|p| crate::full_cmp::processing_result_bytes(&p.result))
            .unwrap_or(0)
        + 256
}

/// Percentile helper over an unsorted sample.
fn pct(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[derive(Default)]
struct Interval {
    matched: u64,
    mismatched: u64,
    noframe: u64,
    fl_only: u64,
    agave_only_ran: u64,
    agave_only_skipped: u64,
    late_fl: u64,
    mismatch_classes: HashMap<&'static str, u64>,
    mismatch_programs: HashMap<Pubkey, u64>,
    lead_us: Vec<i64>,
    lead_us_token: Vec<i64>,
    fl_latency_us: Vec<i64>,
    fl_latency_us_token: Vec<i64>,
    agave_latency_us: Vec<i64>,
    agave_latency_us_token: Vec<i64>,
    spec_finals: u64,
    nonspec_finals: u64,
    incarnations: u64,
    finals: u64,
    exec_us: u64,
    runs_started: u64,
    runs_completed: u64,
    runs_aborted: HashMap<&'static str, u64>,
    slots_skipped: HashMap<&'static str, u64>,
    parent_frozen_at_first_set: u64,
    parent_waited: u64,
    parent_wait_us: Vec<i64>,
    slot_checks_ok: u64,
    slot_check_mismatch_keys: u64,
    validation_failures: u64,
    eager_reexecs: u64,
    spec_dispatches: u64,
    nonspec_dispatches: u64,
    runs_chained: u64,
    chained_matched: u64,
    chained_mismatched: u64,
    /// Chained transactions matched and exported with a lead (non-vote).
    chained_lead_us: Vec<i64>,
    sysvar_checks_ok: u64,
    sysvar_check_mismatch: u64,
    predictions: u64,
    shadow_predictions: u64,
    pred_hits: u64,
    pred_misses: u64,
    spec_relaxed: u64,
    final_fixups: u64,
    /// Held FL records / agave frames evicted by the comparator's bounds.
    held_evicted: u64,
}

/// Most pending entries each of the comparator's join maps holds.
const HELD_MAX_COUNT: usize = 200_000;

/// Byte bound of each join map: an eighth of the memory cap.
fn held_max_bytes(config: &Config) -> i64 {
    (config.mem_cap_mb as i64).saturating_mul(1 << 20) / 8
}

struct RunInfo {
    run: Arc<Run>,
    started: Instant,
    completed: bool,
    frozen: Option<BankId>,
}

pub struct Comparator {
    config: Arc<Config>,
    bank_forks: Arc<RwLock<BankForks>>,
    fl: Held<(Slot, Signature), FinalRecord>,
    agave: Held<(Slot, Signature), AgaveFrame>,
    /// Slots FL started a run for since it was last (re)enabled: the first and the highest.
    /// Agave frames/results of other slots are dropped at once (FL has, or will have, no
    /// result for them: catch-up backlog after a restart, skipped slots).
    first_run_slot: Option<Slot>,
    max_run_slot: Slot,
    runs: HashMap<RunId, RunInfo>,
    slot_runs: HashMap<Slot, RunId>,
    skipped: HashMap<Slot, &'static str>,
    interval: Interval,
    interval_start: Instant,
    export: Option<RotatingWriter>,
    mismatches: Option<RotatingWriter>,
    summaries: Option<RotatingWriter>,
    samples_this_minute: u32,
    minute_start: Instant,
    pub tap_stats: Option<Arc<crate::Shared>>,
    pub sink_drops: Arc<AtomicU64>,
    /// Phase-3 output ring statistics (cumulative; reported per interval).
    pub out_stats: Option<Arc<crate::output::OutStats>>,
    out_prev: [u64; 16],
    totals: Interval,
    /// Commit-mode shadow check (full processing results, `full_cmp`).
    pub full: FullCompare,
    /// Commit mode: the committer's counters (reported as `fast_lane_commit` lines).
    pub commit_metrics: Option<Arc<crate::commit::CommitMetrics>>,
    /// Sampled per-transaction commit attribution (`fl_commit.*.csv`).
    commit_export: Option<RotatingWriter>,
    export_dir: PathBuf,
    commit_report: crate::commit::CommitReport,
    /// Agave captures dropped because the queue to the comparator was full.
    pub full_drops: Arc<AtomicU64>,
}

impl Comparator {
    pub fn new(
        config: Arc<Config>,
        bank_forks: Arc<RwLock<BankForks>>,
        export_dir: PathBuf,
        sink_drops: Arc<AtomicU64>,
    ) -> Self {
        let mb = config.export_file_mb.saturating_mul(1 << 20);
        let export = RotatingWriter::new(
            &export_dir,
            "fl_tx",
            "csv",
            Some(
                "slot,ordinal,signature,vote,token,outcome,class,fl_final_unix_ns,\
                 agave_unix_ns,tap_unix_ns,lead_us,fl_latency_us,agave_latency_us,\
                 incarnations,spec,exec_us,n_preds,kind,ok,parent_slot,ingest_us,\
                 first_dispatch_us,exec_start_us,exec_end_us,src,chained,ready_us,rebased",
            ),
            mb,
            config.export_files,
        )
        .map_err(|err| warn!("fast lane: export disabled: {err}"))
        .ok();
        let mismatches =
            RotatingWriter::new(&export_dir, "fl_mismatch", "jsonl", None, 64 << 20, 4)
                .map_err(|err| warn!("fast lane: mismatch samples disabled: {err}"))
                .ok();
        let summaries =
            RotatingWriter::new(&export_dir, "fl_summary", "jsonl", None, 64 << 20, 4)
                .map_err(|err| warn!("fast lane: summaries disabled: {err}"))
                .ok();
        info!("fast lane: comparator exporting to {}", export_dir.display());
        let held_bytes = held_max_bytes(&config);
        Self {
            fl: Held::new(HELD_MAX_COUNT, held_bytes, &crate::mem::CMP_HELD_BYTES),
            agave: Held::new(HELD_MAX_COUNT, held_bytes, &crate::mem::CMP_HELD_BYTES),
            full: FullCompare::with_max_bytes(held_bytes),
            config,
            bank_forks,
            first_run_slot: None,
            max_run_slot: 0,
            runs: HashMap::new(),
            slot_runs: HashMap::new(),
            skipped: HashMap::new(),
            interval: Interval::default(),
            interval_start: Instant::now(),
            export,
            mismatches,
            summaries,
            samples_this_minute: 0,
            minute_start: Instant::now(),
            tap_stats: None,
            sink_drops,
            out_stats: None,
            out_prev: [0; 16],
            totals: Interval::default(),
            full_drops: Arc::new(AtomicU64::new(0)),
            commit_metrics: None,
            commit_report: Default::default(),
            commit_export: None,
            export_dir: export_dir.clone(),
        }
    }

    pub fn run_loop(
        &mut self,
        cmp_rx: Receiver<CmpMsg>,
        frame_rx: Receiver<AgaveFrame>,
        full_rx: Receiver<Box<AgaveProcessed>>,
        exit: Arc<AtomicBool>,
    ) {
        let mut last_gc = Instant::now();
        loop {
            if exit.load(Ordering::Relaxed) {
                self.flush();
                return;
            }
            select! {
                recv(cmp_rx) -> msg => match msg {
                    Ok(msg) => self.on_msg(msg),
                    Err(_) => { self.flush(); return; }
                },
                recv(frame_rx) -> msg => match msg {
                    Ok(frame) => {
                        crate::mem::FRAME_QUEUE_BYTES.sub(agave_frame_bytes(&frame));
                        self.on_agave(frame);
                    }
                    Err(_) => { self.flush(); return; }
                },
                recv(full_rx) -> msg => match msg {
                    Ok(processed) => {
                        crate::mem::FULL_BYTES.sub(crate::full_cmp::agave_bytes(&processed));
                        self.on_agave_processed(processed);
                    }
                    Err(_) => { self.flush(); return; }
                },
                default(Duration::from_millis(100)) => {},
            }
            if !crate::control::is_active() {
                self.release();
            }
            if last_gc.elapsed() >= Duration::from_millis(500) {
                self.gc();
                last_gc = Instant::now();
            }
            if self.interval_start.elapsed()
                >= Duration::from_secs(self.config.summary_interval_s.max(1))
            {
                self.summary();
            }
        }
    }

    /// Drop everything held (the fast lane is off): pending frames and records, and runs.
    fn release(&mut self) {
        if self.full.fl_len() + self.full.agave_len() > 0 {
            self.full.release();
        }
        self.first_run_slot = None;
        self.max_run_slot = 0;
        if self.fl.is_empty() && self.agave.is_empty() && self.runs.is_empty() {
            return;
        }
        self.fl.clear();
        self.agave.clear();
        self.runs.clear();
        self.slot_runs.clear();
    }

    /// Whether agave's frame or result for `slot` can still meet an FL result: FL has a run for
    /// the slot, or may still start one (a slot above every run FL started since it was
    /// enabled, and not skipped). Everything else (the catch-up backlog after a restart, slots
    /// FL skipped or passed) is dropped at once and counted as unjoined.
    fn wants_agave_slot(&self, slot: Slot) -> bool {
        if self.slot_runs.contains_key(&slot) {
            return true;
        }
        if self.skipped.contains_key(&slot) {
            return false;
        }
        self.first_run_slot.is_some() && slot > self.max_run_slot
    }

    fn flush(&mut self) {
        for w in [
            &mut self.export,
            &mut self.mismatches,
            &mut self.summaries,
            &mut self.commit_export,
        ]
        .into_iter()
            .flatten()
        {
            w.flush();
        }
    }

    pub fn on_msg(&mut self, msg: CmpMsg) {
        match msg {
            CmpMsg::Final(record) => {
                crate::mem::CMP_QUEUE_BYTES.sub(record_bytes(&record));
                if crate::control::is_active() {
                    self.on_final(*record);
                }
            }
            CmpMsg::RunStart {
                run_id,
                run,
                parent_frozen_at_first_set,
                parent_wait_us,
                ctx_build_us: _,
            } => {
                self.interval.runs_started += 1;
                if run.chain.is_some() {
                    self.interval.runs_chained += 1;
                }
                if parent_frozen_at_first_set {
                    self.interval.parent_frozen_at_first_set += 1;
                } else {
                    self.interval.parent_waited += 1;
                    self.interval.parent_wait_us.push(parent_wait_us as i64);
                }
                self.slot_runs.insert(run.slot, run_id);
                self.first_run_slot = Some(self.first_run_slot.map_or(run.slot, |f| f.min(run.slot)));
                self.max_run_slot = self.max_run_slot.max(run.slot);
                self.runs.insert(
                    run_id,
                    RunInfo {
                        run,
                        started: Instant::now(),
                        completed: false,
                        frozen: None,
                    },
                );
            }
            CmpMsg::RunEnd { run_id, summary } => {
                self.interval.validation_failures += summary.validation_failures;
                self.interval.eager_reexecs += summary.eager_reexecs;
                self.interval.spec_dispatches += summary.spec_dispatches;
                self.interval.nonspec_dispatches += summary.nonspec_dispatches;
                self.interval.predictions += summary.predictions;
                self.interval.shadow_predictions += summary.shadow_predictions;
                self.interval.pred_hits += summary.pred_hits;
                self.interval.pred_misses += summary.pred_misses;
                self.interval.spec_relaxed += summary.spec_relaxed;
                self.interval.final_fixups += summary.final_fixups;
                match summary.aborted {
                    Some(reason) => {
                        *self.interval.runs_aborted.entry(reason).or_default() += 1;
                        if let Some(info) = self.runs.remove(&run_id) {
                            self.slot_runs.remove(&info.run.slot);
                            self.skipped.insert(info.run.slot, reason);
                        }
                    }
                    None => {
                        self.interval.runs_completed += 1;
                        if let Some(info) = self.runs.get_mut(&run_id) {
                            info.completed = true;
                        }
                        self.maybe_slot_check(run_id);
                    }
                }
            }
            CmpMsg::SlotSkipped { slot, reason } => {
                *self.interval.slots_skipped.entry(reason).or_default() += 1;
                self.skipped.insert(slot, reason);
            }
            CmpMsg::SlotFrozen { slot, bank_id, .. } => {
                if let Some(&run_id) = self.slot_runs.get(&slot) {
                    if let Some(info) = self.runs.get_mut(&run_id) {
                        info.frozen = Some(bank_id);
                    }
                    self.maybe_slot_check(run_id);
                }
            }
            CmpMsg::Rooted { slot } => {
                self.skipped.retain(|s, _| *s + 64 > slot);
            }
            CmpMsg::Commit(timing) => {
                if self.commit_export.is_none() {
                    self.commit_export = RotatingWriter::new(
                        &self.export_dir,
                        "fl_commit",
                        "csv",
                        Some(crate::commit::CommitTiming::csv_header()),
                        self.config.export_file_mb.saturating_mul(1 << 20),
                        self.config.export_files,
                    )
                    .map_err(|err| warn!("fast lane: commit export disabled: {err}"))
                    .ok();
                }
                if let Some(w) = self.commit_export.as_mut() {
                    w.write_line(&timing.csv_line());
                }
            }
        }
    }

    /// Agave's processing result of a replayed transaction (commit mode shadow).
    pub fn on_agave_processed(&mut self, processed: Box<AgaveProcessed>) {
        if !crate::control::is_active()
            || crate::control::commit_mode() != crate::control::COMMIT_SHADOW
        {
            return;
        }
        self.accept_agave_processed(processed);
    }

    fn accept_agave_processed(&mut self, processed: Box<AgaveProcessed>) {
        if !self.wants_agave_slot(processed.slot) {
            // FL has (or will have) no result for this slot.
            self.full.interval.unjoined_agave += 1;
            return;
        }
        if let Some(mismatch) = self.full.on_agave(processed) {
            self.write_sample(mismatch.json);
        }
    }

    fn write_sample(&mut self, line: String) {
        if self.minute_start.elapsed() >= Duration::from_secs(60) {
            self.minute_start = Instant::now();
            self.samples_this_minute = 0;
        }
        if self.samples_this_minute >= self.config.mismatch_samples_per_min {
            return;
        }
        self.samples_this_minute += 1;
        if let Some(w) = self.mismatches.as_mut() {
            w.write_line(&line);
        }
    }

    fn on_final(&mut self, mut record: FinalRecord) {
        if let Some(processed) = record.outcome.processed.take() {
            if crate::control::commit_mode() == crate::control::COMMIT_SHADOW {
                let outcome = &record.outcome;
                let fl = FlFull::new(
                    outcome.slot,
                    outcome.parent_slot,
                    outcome.ordinal,
                    outcome.signature,
                    *processed,
                );
                if let Some(mismatch) = self.full.on_fl(fl) {
                    self.write_sample(mismatch.json);
                }
            }
        }
        let interval = &mut self.interval;
        interval.finals += 1;
        interval.incarnations += u64::from(record.incarnations);
        interval.exec_us += record
            .t_exec_end
            .saturating_duration_since(record.t_exec_start)
            .as_micros() as u64;
        if record.speculative {
            interval.spec_finals += 1;
        } else {
            interval.nonspec_finals += 1;
        }
        let key = (record.outcome.slot, record.outcome.signature);
        if let Some(frame) = self.agave.remove(&key) {
            // Agave was first: FL is late for this transaction.
            self.interval.late_fl += 1;
            self.join(record, Some(frame));
        } else if crate::control::commit_mode() == crate::control::COMMIT_ON {
            // Commit mode: agave's grouped notification of this transaction comes from FL's
            // own commit, so there is nothing to wait for.
            self.join(record, None);
        } else {
            let bytes = record_bytes(&record);
            for evicted in self.fl.insert(key, record, bytes) {
                self.interval.held_evicted += 1;
                self.join(evicted, None);
            }
        }
    }

    fn on_agave(&mut self, frame: AgaveFrame) {
        if !crate::control::is_active() {
            return;
        }
        self.accept_agave_frame(frame);
    }

    fn accept_agave_frame(&mut self, frame: AgaveFrame) {
        let key = (frame.slot, frame.signature);
        if let Some(record) = self.fl.remove(&key) {
            self.join(record, Some(frame));
        } else if !self.wants_agave_slot(frame.slot) {
            self.interval.agave_only_skipped += 1;
        } else {
            let bytes = agave_frame_bytes(&frame);
            let evicted = self.agave.insert(key, frame, bytes).len() as u64;
            self.interval.held_evicted += evicted;
            self.interval.agave_only_ran += evicted;
        }
    }

    fn classify(
        fl: &Option<Vec<(Pubkey, AccountSharedData)>>,
        agave: &[(Pubkey, AccountSharedData)],
    ) -> Option<&'static str> {
        let Some(fl) = fl else {
            return Some("presence");
        };
        if fl.len() != agave.len() || fl.iter().zip(agave).any(|((a, _), (b, _))| a != b) {
            return Some("keys");
        }
        for ((_, a), (_, b)) in fl.iter().zip(agave) {
            if accounts_equal(a, b) {
                continue;
            }
            if a.owner() != b.owner() {
                return Some("owner");
            }
            if a.data() != b.data() {
                return Some("data");
            }
            if a.lamports() != b.lamports() {
                return Some("lamports");
            }
            if a.rent_epoch() != b.rent_epoch() {
                return Some("rent_epoch");
            }
            return Some("executable");
        }
        None
    }

    fn is_token_frame(accounts: &[(Pubkey, AccountSharedData)]) -> bool {
        accounts
            .iter()
            .any(|(_, a)| a.owner() == &TOKEN_PROGRAM || a.owner() == &TOKEN_2022_PROGRAM)
    }

    fn join(&mut self, record: FinalRecord, frame: Option<AgaveFrame>) {
        let outcome = &record.outcome;
        let (outcome_label, class, agave_unix_ns, lead_us, agave_latency_us, token) = match &frame
        {
            Some(frame) => {
                let class = Self::classify(&outcome.frame, &frame.accounts);
                let token = Self::is_token_frame(&frame.accounts);
                let lead = frame.t.saturating_duration_since(record.t_final).as_micros() as i64
                    - record.t_final.saturating_duration_since(frame.t).as_micros() as i64;
                let agave_latency = frame
                    .t
                    .saturating_duration_since(outcome.t_tap)
                    .as_micros() as i64;
                (
                    if class.is_none() { "match" } else { "mismatch" },
                    class,
                    frame.t_unix_ns,
                    Some(lead),
                    Some(agave_latency),
                    token,
                )
            }
            None => {
                let token = outcome
                    .frame
                    .as_deref()
                    .map(Self::is_token_frame)
                    .unwrap_or(false);
                if outcome.frame.is_none() {
                    ("noframe", None, 0, None, None, token)
                } else {
                    ("fl_only", None, 0, None, None, token)
                }
            }
        };
        let fl_latency_us = record
            .t_final
            .saturating_duration_since(outcome.t_tap)
            .as_micros() as i64;
        let interval = &mut self.interval;
        match outcome_label {
            "match" => interval.matched += 1,
            "mismatch" => interval.mismatched += 1,
            "noframe" => interval.noframe += 1,
            _ => interval.fl_only += 1,
        }
        if outcome.chained {
            match outcome_label {
                "match" => interval.chained_matched += 1,
                "mismatch" => interval.chained_mismatched += 1,
                _ => {}
            }
            if let (false, "match", Some(lead)) = (outcome.is_vote, outcome_label, lead_us) {
                interval.chained_lead_us.push(lead);
            }
        }
        if let Some(class) = class {
            *interval.mismatch_classes.entry(class).or_default() += 1;
            for program in &outcome.programs {
                *interval.mismatch_programs.entry(*program).or_default() += 1;
            }
        }
        if !outcome.is_vote && outcome_label == "match" {
            if let Some(lead) = lead_us {
                interval.lead_us.push(lead);
                interval.fl_latency_us.push(fl_latency_us);
                interval.agave_latency_us.push(agave_latency_us.unwrap_or(0));
                if token {
                    interval.lead_us_token.push(lead);
                    interval.fl_latency_us_token.push(fl_latency_us);
                    interval
                        .agave_latency_us_token
                        .push(agave_latency_us.unwrap_or(0));
                }
            }
        } else if !outcome.is_vote
            && outcome_label == "fl_only"
            && crate::control::commit_mode() == crate::control::COMMIT_ON
        {
            // Commit mode: no agave frame to join (FL's commit produced it); FL's own latency.
            interval.fl_latency_us.push(fl_latency_us);
            if token {
                interval.fl_latency_us_token.push(fl_latency_us);
            }
        }
        if class.is_some() {
            self.sample_mismatch(&record, frame.as_ref(), class.unwrap_or("?"));
        }
        if outcome.is_vote && !self.config.export_votes {
            return;
        }
        if let Some(export) = self.export.as_mut() {
            let exec_us = record
                .t_exec_end
                .saturating_duration_since(record.t_exec_start)
                .as_micros();
            let since_tap = |t: Instant| t.saturating_duration_since(outcome.t_tap).as_micros();
            let line = format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:?},{},{},{},{},{},{},{},{},{},{}",
                outcome.slot,
                outcome.ordinal,
                outcome.signature,
                u8::from(outcome.is_vote),
                u8::from(token),
                outcome_label,
                class.unwrap_or(""),
                record.t_final_unix_ns,
                agave_unix_ns,
                outcome.t_tap_unix_ns,
                lead_us.map(|v| v.to_string()).unwrap_or_default(),
                fl_latency_us,
                agave_latency_us.map(|v| v.to_string()).unwrap_or_default(),
                record.incarnations,
                u8::from(record.speculative),
                exec_us,
                record.n_preds,
                outcome.kind,
                u8::from(outcome.status.is_ok()),
                outcome.parent_slot,
                since_tap(record.t_ingest),
                since_tap(record.t_first_dispatch),
                since_tap(record.t_exec_start),
                since_tap(record.t_exec_end),
                if outcome.from_ring { "ring" } else { "blockstore" },
                u8::from(outcome.chained),
                since_tap(record.t_ready),
                record.rebased,
            );
            export.write_line(&line);
        }
    }

    fn account_json(out: &mut String, account: &AccountSharedData) {
        let data = account.data();
        let hash = data
            .iter()
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3));
        let _ = write!(
            out,
            "{{\"lamports\":{},\"owner\":\"{}\",\"exec\":{},\"rent_epoch\":{},\"len\":{},\"fnv\":\"{hash:016x}\"}}",
            account.lamports(),
            account.owner(),
            account.executable(),
            account.rent_epoch(),
            data.len(),
        );
    }

    fn sample_mismatch(&mut self, record: &FinalRecord, frame: Option<&AgaveFrame>, class: &str) {
        if self.minute_start.elapsed() >= Duration::from_secs(60) {
            self.minute_start = Instant::now();
            self.samples_this_minute = 0;
        }
        if self.samples_this_minute >= self.config.mismatch_samples_per_min {
            return;
        }
        self.samples_this_minute += 1;
        let outcome = &record.outcome;
        let mut s = String::with_capacity(2048);
        let _ = write!(
            s,
            "{{\"slot\":{},\"parent\":{},\"ordinal\":{},\"signature\":\"{}\",\"class\":\"{class}\",\
             \"kind\":\"{:?}\",\"status\":\"{:?}\",\"incarnations\":{},\"spec\":{},\"programs\":[",
            outcome.slot,
            outcome.parent_slot,
            outcome.ordinal,
            outcome.signature,
            outcome.kind,
            outcome.status,
            record.incarnations,
            record.speculative,
        );
        for (i, p) in outcome.programs.iter().enumerate() {
            let _ = write!(s, "{}\"{p}\"", if i > 0 { "," } else { "" });
        }
        s.push_str("],\"accounts\":[");
        let fl = outcome.frame.clone().unwrap_or_default();
        let agave = frame.map(|f| f.accounts.clone()).unwrap_or_default();
        let mut keys: Vec<Pubkey> = fl.iter().map(|(k, _)| *k).collect();
        for (k, _) in &agave {
            if !keys.contains(k) {
                keys.push(*k);
            }
        }
        for (i, key) in keys.iter().enumerate() {
            let a = fl.iter().find(|(k, _)| k == key).map(|(_, a)| a);
            let b = agave.iter().find(|(k, _)| k == key).map(|(_, a)| a);
            let equal = match (a, b) {
                (Some(a), Some(b)) => accounts_equal(a, b),
                _ => false,
            };
            let _ = write!(
                s,
                "{}{{\"pubkey\":\"{key}\",\"equal\":{equal}",
                if i > 0 { "," } else { "" }
            );
            if let Some(a) = a {
                s.push_str(",\"fl\":");
                Self::account_json(&mut s, a);
            }
            if let Some(b) = b {
                s.push_str(",\"agave\":");
                Self::account_json(&mut s, b);
            }
            if let (Some(a), Some(b)) = (a, b) {
                if let Some(off) = a.data().iter().zip(b.data()).position(|(x, y)| x != y) {
                    let _ = write!(s, ",\"first_diff\":{off}");
                }
            }
            s.push('}');
        }
        s.push_str("]}");
        if let Some(w) = self.mismatches.as_mut() {
            w.write_line(&s);
        }
    }

    /// Slot-level check: once FL's run is complete and agave froze the slot, every account
    /// FL wrote must equal the frozen bank's value (fee collector, incinerator and
    /// SlotHistory excepted: agave writes them at freeze). The run's Clock and SlotHashes
    /// (for a chained run: resolved when the parent froze) must equal the bank's.
    fn maybe_slot_check(&mut self, run_id: RunId) {
        let Some(info) = self.runs.get(&run_id) else {
            return;
        };
        let (Some(bank_id), true) = (info.frozen, info.completed) else {
            return;
        };
        let Some(info) = self.runs.remove(&run_id) else {
            return;
        };
        self.slot_runs.remove(&info.run.slot);
        let bank = match self.bank_forks.read() {
            Ok(forks) => forks.get(info.run.slot),
            Err(_) => None,
        };
        let Some(bank) = bank.filter(|b| b.bank_id() == bank_id) else {
            return;
        };
        let collector = bank.fast_lane_collector_id();
        let clock_id = solana_sdk_ids::sysvar::clock::id();
        let slot_hashes_id = solana_sdk_ids::sysvar::slot_hashes::id();
        let run_slot_hashes = match &info.run.chain {
            Some(_) => info.run.overlay.latest(&slot_hashes_id).value,
            None => Some(info.run.ctx.slot_hashes_account.clone()),
        };
        let sysvars_ok = same_value(
            &Some(info.run.ctx.clock_account.clone()),
            &bank.get_account(&clock_id),
        ) && same_value(&run_slot_hashes, &bank.get_account(&slot_hashes_id));
        if sysvars_ok {
            self.interval.sysvar_checks_ok += 1;
        } else {
            self.interval.sysvar_check_mismatch += 1;
            warn!(
                "fast lane: slot {} sysvar check failed (chained {})",
                info.run.slot,
                info.run.chain.is_some()
            );
        }
        let mut bad = 0u64;
        for key in info.run.overlay.written_keys().into_iter().take(10_000) {
            if Some(key) == collector
                || key == incinerator::id()
                || key == solana_sdk_ids::sysvar::slot_history::id()
            {
                continue;
            }
            let fl = info.run.overlay.latest(&key).value;
            let agave = bank.get_account(&key);
            if !same_value(&fl, &agave) {
                bad += 1;
                if bad <= 3 {
                    warn!(
                        "fast lane: slot {} check: {key} differs (fl {:?} vs agave {:?} lamports)",
                        info.run.slot,
                        fl.as_ref().map(|a| a.lamports()),
                        agave.as_ref().map(|a| a.lamports())
                    );
                }
            }
        }
        if bad == 0 {
            self.interval.slot_checks_ok += 1;
        } else {
            self.interval.slot_check_mismatch_keys += bad;
        }
    }

    fn gc(&mut self) {
        let horizon = Duration::from_secs(3);
        self.full.gc(horizon);
        if crate::control::commit_mode() != crate::control::COMMIT_SHADOW
            && self.full.fl_len() + self.full.agave_len() > 0
        {
            self.full.release();
        }
        let now = Instant::now();
        let stale_fl = self
            .fl
            .remove_where(|_, r| now.saturating_duration_since(r.t_final) > horizon);
        for (_, record) in stale_fl {
            self.join(record, None);
        }
        let stale_agave = self
            .agave
            .remove_where(|_, f| now.saturating_duration_since(f.t) > horizon);
        for (key, _) in stale_agave {
            if self.slot_runs.contains_key(&key.0) {
                self.interval.agave_only_ran += 1;
            } else {
                self.interval.agave_only_skipped += 1;
            }
        }
        // Skipped-slot memory is bounded (rooted events do not reach the comparator).
        while self.skipped.len() > 4096 {
            let Some(&oldest) = self.skipped.keys().min() else {
                break;
            };
            self.skipped.remove(&oldest);
        }
        // Runs that never got both "complete" and "frozen" are dropped after a while.
        let stale_runs: Vec<RunId> = self
            .runs
            .iter()
            .filter(|(_, info)| info.started.elapsed() > Duration::from_secs(20))
            .map(|(id, _)| *id)
            .collect();
        for run_id in stale_runs {
            if let Some(info) = self.runs.remove(&run_id) {
                self.slot_runs.remove(&info.run.slot);
            }
        }
    }

    fn summary(&mut self) {
        let secs = self.interval_start.elapsed().as_secs_f64();
        let mut iv = std::mem::take(&mut self.interval);
        let fiv = self.full.take_interval();
        let full_fields = fiv.fields_sorted();
        let full_drops = self.full_drops.swap(0, Ordering::Relaxed);
        self.interval_start = Instant::now();
        for v in [
            &mut iv.lead_us,
            &mut iv.lead_us_token,
            &mut iv.fl_latency_us,
            &mut iv.fl_latency_us_token,
            &mut iv.agave_latency_us,
            &mut iv.agave_latency_us_token,
            &mut iv.parent_wait_us,
            &mut iv.chained_lead_us,
        ] {
            v.sort_unstable();
        }
        let mem = crate::mem::Snapshot::now();
        let cap_trips = crate::mem::CAP_TRIPS.load(Ordering::Relaxed);
        let compared = iv.matched + iv.mismatched;
        let exact = if compared > 0 {
            iv.matched as f64 / compared as f64
        } else {
            0.0
        };
        let tap = self.tap_stats.as_ref().map(|s| &s.tap_stats);
        let tap_drops = tap.map(|t| t.tap_drops.load(Ordering::Relaxed)).unwrap_or(0);
        let frame_drops = tap
            .map(|t| t.agave_frame_drops.load(Ordering::Relaxed))
            .unwrap_or(0);
        let mut classes: Vec<_> = iv.mismatch_classes.iter().collect();
        classes.sort();
        let mut programs: Vec<_> = iv.mismatch_programs.iter().collect();
        programs.sort_by(|a, b| b.1.cmp(a.1));
        programs.truncate(5);
        let mut skipped: Vec<_> = iv.slots_skipped.iter().collect();
        skipped.sort();
        let mut aborted: Vec<_> = iv.runs_aborted.iter().collect();
        aborted.sort();
        info!(
            "fast_lane_summary secs={secs:.1} compared={compared} match={} mismatch={} \
             exact={exact:.6} noframe={} fl_only={} agave_only_ran={} agave_only_skipped={} \
             late_fl={} token_lead_us_p50={} p90={} token_fl_lat_us_p50={} p90={} \
             token_agave_lat_us_p50={} p90={} all_lead_us_p50={} p90={} finals={} \
             spec_finals={} incarnations_per_tx={:.3} val_fail={} eager={} runs={} done={} \
             aborted={aborted:?} skipped={skipped:?} parent_frozen={} parent_waited={} \
             parent_wait_us_p50={} slot_checks_ok={} slot_check_bad_keys={} classes={classes:?} \
             top_programs={programs:?} tap_drops={tap_drops} frame_drops={frame_drops} \
             cmp_drops={} chained_runs={} chained_match={} chained_mismatch={} \
             chained_lead_us_p50={} p90={} sysvar_ok={} sysvar_bad={} \
             mem_total_mb={} mem_overlay_mb={} live_runs={} mem_frame_queue_mb={} \
             mem_cmp_queue_mb={} mem_cmp_held_mb={} cmp_fl={} cmp_agave={} cmp_runs={} \
             mem_ingest_pending_kb={} banks_held={} program_entries={} program_mb={} \
             hints_kb={} mem_pred_kb={} mem_vm_pool_kb={} cap_trips={} rebase_preds={} rebase_shadow={} \
             rebase_hit={} rebase_miss={} spec_relaxed={} final_fixups={} commit={} \
             full_cmp={} full_match={} full_mismatch={} full_fields={full_fields:?} \
             full_unjoined_fl={} full_unjoined_agave={} full_unrecorded={} \
             full_drops={full_drops} mem_full_kb={} cluster_matched={} cluster_mismatched={} \
             poisoned={} sticky={} held_evicted={} full_evicted={} cap_reenables={}",
            iv.matched,
            iv.mismatched,
            iv.noframe,
            iv.fl_only,
            iv.agave_only_ran,
            iv.agave_only_skipped,
            iv.late_fl,
            pct(&iv.lead_us_token, 0.5),
            pct(&iv.lead_us_token, 0.9),
            pct(&iv.fl_latency_us_token, 0.5),
            pct(&iv.fl_latency_us_token, 0.9),
            pct(&iv.agave_latency_us_token, 0.5),
            pct(&iv.agave_latency_us_token, 0.9),
            pct(&iv.lead_us, 0.5),
            pct(&iv.lead_us, 0.9),
            iv.finals,
            iv.spec_finals,
            if iv.finals > 0 {
                iv.incarnations as f64 / iv.finals as f64
            } else {
                0.0
            },
            iv.validation_failures,
            iv.eager_reexecs,
            iv.runs_started,
            iv.runs_completed,
            iv.parent_frozen_at_first_set,
            iv.parent_waited,
            pct(&iv.parent_wait_us, 0.5),
            iv.slot_checks_ok,
            iv.slot_check_mismatch_keys,
            self.sink_drops.load(Ordering::Relaxed),
            iv.runs_chained,
            iv.chained_matched,
            iv.chained_mismatched,
            pct(&iv.chained_lead_us, 0.5),
            pct(&iv.chained_lead_us, 0.9),
            iv.sysvar_checks_ok,
            iv.sysvar_check_mismatch,
            mem.total_bytes() >> 20,
            mem.overlay >> 20,
            mem.live_runs,
            mem.frame_queue >> 20,
            mem.cmp_queue >> 20,
            mem.cmp_held >> 20,
            self.fl.len(),
            self.agave.len(),
            self.runs.len(),
            mem.ingest_pending >> 10,
            mem.banks_held,
            mem.program_entries,
            mem.program >> 20,
            mem.hints >> 10,
            mem.pred >> 10,
            mem.vm_pool >> 10,
            cap_trips,
            iv.predictions,
            iv.shadow_predictions,
            iv.pred_hits,
            iv.pred_misses,
            iv.spec_relaxed,
            iv.final_fixups,
            crate::control::commit_mode_name(crate::control::commit_mode()),
            fiv.compared,
            fiv.matched,
            fiv.mismatched,
            fiv.unjoined_fl,
            fiv.unjoined_agave,
            fiv.unrecorded,
            mem.full >> 10,
            crate::cluster_check::totals().0,
            crate::cluster_check::totals().1,
            crate::control::is_poisoned(),
            crate::control::is_poisoned_sticky(),
            iv.held_evicted,
            fiv.evicted,
            crate::mem::CAP_REENABLES.load(Ordering::Relaxed),
        );
        solana_metrics::datapoint_info!(
            "fast_lane_full",
            ("compared", fiv.compared as i64, i64),
            ("matched", fiv.matched as i64, i64),
            ("mismatched", fiv.mismatched as i64, i64),
            ("unjoined_fl", fiv.unjoined_fl as i64, i64),
            ("unjoined_agave", fiv.unjoined_agave as i64, i64),
            ("unrecorded", fiv.unrecorded as i64, i64),
            ("drops", full_drops as i64, i64),
            ("full_bytes", mem.full, i64),
        );
        solana_metrics::datapoint_info!(
            "fast_lane",
            ("match", iv.matched as i64, i64),
            ("mismatch", iv.mismatched as i64, i64),
            ("noframe", iv.noframe as i64, i64),
            ("fl_only", iv.fl_only as i64, i64),
            ("agave_only_ran", iv.agave_only_ran as i64, i64),
            ("agave_only_skipped", iv.agave_only_skipped as i64, i64),
            ("late_fl", iv.late_fl as i64, i64),
            ("token_lead_us_p50", pct(&iv.lead_us_token, 0.5), i64),
            ("token_lead_us_p90", pct(&iv.lead_us_token, 0.9), i64),
            ("token_fl_latency_us_p50", pct(&iv.fl_latency_us_token, 0.5), i64),
            ("token_fl_latency_us_p90", pct(&iv.fl_latency_us_token, 0.9), i64),
            ("token_fl_latency_us_p99", pct(&iv.fl_latency_us_token, 0.99), i64),
            ("token_agave_latency_us_p50", pct(&iv.agave_latency_us_token, 0.5), i64),
            ("token_agave_latency_us_p90", pct(&iv.agave_latency_us_token, 0.9), i64),
            ("token_agave_latency_us_p99", pct(&iv.agave_latency_us_token, 0.99), i64),
            ("finals", iv.finals as i64, i64),
            ("spec_finals", iv.spec_finals as i64, i64),
            ("incarnations", iv.incarnations as i64, i64),
            ("validation_failures", iv.validation_failures as i64, i64),
            ("eager_reexecs", iv.eager_reexecs as i64, i64),
            ("runs_started", iv.runs_started as i64, i64),
            ("runs_completed", iv.runs_completed as i64, i64),
            ("parent_waited", iv.parent_waited as i64, i64),
            ("slot_checks_ok", iv.slot_checks_ok as i64, i64),
            ("slot_check_bad_keys", iv.slot_check_mismatch_keys as i64, i64),
            ("exec_us", iv.exec_us as i64, i64),
            ("tap_drops", tap_drops as i64, i64),
            ("frame_drops", frame_drops as i64, i64),
            ("runs_chained", iv.runs_chained as i64, i64),
            ("chained_match", iv.chained_matched as i64, i64),
            ("chained_mismatch", iv.chained_mismatched as i64, i64),
            ("sysvar_checks_ok", iv.sysvar_checks_ok as i64, i64),
            ("sysvar_check_mismatch", iv.sysvar_check_mismatch as i64, i64),
            ("rebase_predictions", iv.predictions as i64, i64),
            ("rebase_shadow_predictions", iv.shadow_predictions as i64, i64),
            ("rebase_hits", iv.pred_hits as i64, i64),
            ("rebase_misses", iv.pred_misses as i64, i64),
            ("spec_relaxed", iv.spec_relaxed as i64, i64),
            ("final_fixups", iv.final_fixups as i64, i64),
        );
        solana_metrics::datapoint_info!(
            "fast_lane_mem",
            ("total_bytes", mem.total_bytes(), i64),
            ("overlay_bytes", mem.overlay, i64),
            ("live_runs", mem.live_runs, i64),
            ("frame_queue_bytes", mem.frame_queue, i64),
            ("cmp_queue_bytes", mem.cmp_queue, i64),
            ("cmp_held_bytes", mem.cmp_held, i64),
            ("cmp_fl", self.fl.len() as i64, i64),
            ("cmp_agave", self.agave.len() as i64, i64),
            ("cmp_runs", self.runs.len() as i64, i64),
            ("ingest_pending_bytes", mem.ingest_pending, i64),
            ("banks_held", mem.banks_held, i64),
            ("program_entries", mem.program_entries, i64),
            ("program_bytes", mem.program, i64),
            ("hint_bytes", mem.hints, i64),
            ("pred_bytes", mem.pred, i64),
            ("vm_pool_bytes", mem.vm_pool, i64),
            ("cap_trips", cap_trips as i64, i64),
        );
        if let Some(w) = self.summaries.as_mut() {
            let line = format!(
                "{{\"unix_ns\":{},\"secs\":{secs:.3},\"match\":{},\"mismatch\":{},\"noframe\":{},\
                 \"fl_only\":{},\"agave_only_ran\":{},\"agave_only_skipped\":{},\"late_fl\":{},\
                 \"token_lead_us\":[{},{},{},{}],\"token_fl_latency_us\":[{},{},{},{}],\
                 \"token_agave_latency_us\":[{},{},{},{}],\"finals\":{},\"spec_finals\":{},\
                 \"incarnations\":{},\"validation_failures\":{},\"eager_reexecs\":{},\
                 \"runs_started\":{},\"runs_completed\":{},\"parent_frozen\":{},\"parent_waited\":{},\
                 \"slot_checks_ok\":{},\"slot_check_bad_keys\":{},\"exec_us\":{},\
                 \"tap_drops\":{tap_drops},\"frame_drops\":{frame_drops},\"runs_chained\":{},\
                 \"chained_match\":{},\"chained_mismatch\":{},\"chained_lead_us\":[{},{},{},{}],\
                 \"sysvar_checks_ok\":{},\"sysvar_check_mismatch\":{},\"mem\":{{\"total\":{},\
                 \"overlay\":{},\"live_runs\":{},\"frame_queue\":{},\"cmp_queue\":{},\"cmp_held\":{},\
                 \"ingest_pending\":{},\"banks_held\":{},\"program_entries\":{},\"program\":{},\
                 \"hints\":{},\"pred\":{},\"vm_pool\":{},\"cap_trips\":{}}},\"rebase\":{{\"predictions\":{},\
                 \"shadow\":{},\"hits\":{},\"misses\":{},\"spec_relaxed\":{},\"final_fixups\":{}}},\
                 \"full\":{{\"compared\":{},\"matched\":{},\"mismatched\":{},\"fields\":{},\
                 \"unjoined_fl\":{},\"unjoined_agave\":{},\"unrecorded\":{},\"drops\":{full_drops},\
                 \"bytes\":{}}}}}",
                unix_ns(),
                iv.matched,
                iv.mismatched,
                iv.noframe,
                iv.fl_only,
                iv.agave_only_ran,
                iv.agave_only_skipped,
                iv.late_fl,
                pct(&iv.lead_us_token, 0.5),
                pct(&iv.lead_us_token, 0.9),
                pct(&iv.lead_us_token, 0.99),
                iv.lead_us_token.len(),
                pct(&iv.fl_latency_us_token, 0.5),
                pct(&iv.fl_latency_us_token, 0.9),
                pct(&iv.fl_latency_us_token, 0.99),
                iv.fl_latency_us_token.len(),
                pct(&iv.agave_latency_us_token, 0.5),
                pct(&iv.agave_latency_us_token, 0.9),
                pct(&iv.agave_latency_us_token, 0.99),
                iv.agave_latency_us_token.len(),
                iv.finals,
                iv.spec_finals,
                iv.incarnations,
                iv.validation_failures,
                iv.eager_reexecs,
                iv.runs_started,
                iv.runs_completed,
                iv.parent_frozen_at_first_set,
                iv.parent_waited,
                iv.slot_checks_ok,
                iv.slot_check_mismatch_keys,
                iv.exec_us,
                iv.runs_chained,
                iv.chained_matched,
                iv.chained_mismatched,
                pct(&iv.chained_lead_us, 0.5),
                pct(&iv.chained_lead_us, 0.9),
                pct(&iv.chained_lead_us, 0.99),
                iv.chained_lead_us.len(),
                iv.sysvar_checks_ok,
                iv.sysvar_check_mismatch,
                mem.total_bytes(),
                mem.overlay,
                mem.live_runs,
                mem.frame_queue,
                mem.cmp_queue,
                mem.cmp_held,
                mem.ingest_pending,
                mem.banks_held,
                mem.program_entries,
                mem.program,
                mem.hints,
                mem.pred,
                mem.vm_pool,
                cap_trips,
                iv.predictions,
                iv.shadow_predictions,
                iv.pred_hits,
                iv.pred_misses,
                iv.spec_relaxed,
                iv.final_fixups,
                fiv.compared,
                fiv.matched,
                fiv.mismatched,
                {
                    let mut fields = String::from("{");
                    for (i, (field, n)) in full_fields.iter().enumerate() {
                        let _ = write!(fields, "{}\"{field}\":{n}", if i > 0 { "," } else { "" });
                    }
                    fields.push('}');
                    fields
                },
                fiv.unjoined_fl,
                fiv.unjoined_agave,
                fiv.unrecorded,
                mem.full,
            );
            w.write_line(&line);
        }
        self.out_summary(secs);
        if let Some(metrics) = self.commit_metrics.clone() {
            let line = self.commit_report.report(secs, &metrics);
            info!("{line}");
            if let Some(w) = self.summaries.as_mut() {
                w.write_line(&format!(
                    "{{\"commit\":true,\"unix_ns\":{},\"line\":\"{}\"}}",
                    unix_ns(),
                    line.replace('"', "'")
                ));
            }
        }
        self.totals.matched += iv.matched;
        self.totals.mismatched += iv.mismatched;
        self.flush();
    }

    /// Output-ring interval statistics: records, bytes, filtered, dropped, and the write
    /// cost on the coordinator (mean, histogram p50/p99 bucket bounds, max).
    fn out_summary(&mut self, secs: f64) {
        let Some(stats) = self.out_stats.as_ref() else {
            return;
        };
        let now: [u64; 16] = [
            stats.tx_records.load(Ordering::Relaxed),
            stats.markers.load(Ordering::Relaxed),
            stats.filtered.load(Ordering::Relaxed),
            stats.bytes.load(Ordering::Relaxed),
            stats.dropped.load(Ordering::Relaxed),
            stats.incomplete.load(Ordering::Relaxed),
            stats.write_ns_sum.load(Ordering::Relaxed),
            0,
            stats.write_ns_hist[0].load(Ordering::Relaxed),
            stats.write_ns_hist[1].load(Ordering::Relaxed),
            stats.write_ns_hist[2].load(Ordering::Relaxed),
            stats.write_ns_hist[3].load(Ordering::Relaxed),
            stats.write_ns_hist[4].load(Ordering::Relaxed),
            stats.write_ns_hist[5].load(Ordering::Relaxed),
            stats.write_ns_hist[6].load(Ordering::Relaxed),
            stats.write_ns_hist[7].load(Ordering::Relaxed),
        ];
        let max_ns = stats.write_ns_max.swap(0, Ordering::Relaxed);
        let d: Vec<u64> = now
            .iter()
            .zip(self.out_prev.iter())
            .map(|(a, b)| a.saturating_sub(*b))
            .collect();
        self.out_prev = now;
        let hist = &d[8..16];
        let n: u64 = hist.iter().sum();
        let bound = |q: f64| -> u64 {
            let target = (n as f64 * q).ceil() as u64;
            let mut acc = 0;
            for (i, c) in hist.iter().enumerate() {
                acc += c;
                if acc >= target.max(1) {
                    return crate::output::WRITE_NS_BUCKETS[i];
                }
            }
            u64::MAX
        };
        let (p50, p99) = if n > 0 { (bound(0.5), bound(0.99)) } else { (0, 0) };
        let mean = if n > 0 { d[6] / n } else { 0 };
        info!(
            "fast_lane_out secs={secs:.1} tx_records={} markers={} filtered={} bytes={} \
             dropped={} incomplete={} write_ns_mean={mean} write_ns_p50_le={p50} \
             write_ns_p99_le={p99} write_ns_max={max_ns} hist={:?}",
            d[0], d[1], d[2], d[3], d[4], d[5], hist
        );
        solana_metrics::datapoint_info!(
            "fast_lane_out",
            ("tx_records", d[0] as i64, i64),
            ("markers", d[1] as i64, i64),
            ("filtered", d[2] as i64, i64),
            ("bytes", d[3] as i64, i64),
            ("dropped", d[4] as i64, i64),
            ("incomplete", d[5] as i64, i64),
            ("write_ns_mean", mean as i64, i64),
            ("write_ns_p50_le", p50.min(i64::MAX as u64) as i64, i64),
            ("write_ns_p99_le", p99.min(i64::MAX as u64) as i64, i64),
            ("write_ns_max", max_ns as i64, i64),
        );
        if let Some(w) = self.summaries.as_mut() {
            w.write_line(&format!(
                "{{\"out\":true,\"unix_ns\":{},\"secs\":{secs:.3},\"tx_records\":{},\"markers\":{},\
                 \"filtered\":{},\"bytes\":{},\"dropped\":{},\"incomplete\":{},\"write_ns_mean\":{mean},\
                 \"write_ns_p50_le\":{p50},\"write_ns_p99_le\":{p99},\"write_ns_max\":{max_ns},\
                 \"write_ns_hist\":{:?}}}",
                unix_ns(),
                d[0], d[1], d[2], d[3], d[4], d[5], hist
            ));
        }
    }

    /// Totals so far (tests).
    pub fn totals(&self) -> (u64, u64) {
        (
            self.totals.matched + self.interval.matched,
            self.totals.mismatched + self.interval.mismatched,
        )
    }
}

#[allow(dead_code)]
fn _kind_is_executed(kind: OutcomeKind) -> bool {
    kind == OutcomeKind::Executed
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_runtime::{bank::Bank, genesis_utils::create_genesis_config},
        solana_svm::transaction_processing_result::ProcessedTransaction,
        solana_svm::account_loader::NoOpTransaction,
        solana_transaction_error::TransactionError,
    };

    fn comparator(mem_cap_mb: usize) -> (Comparator, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let genesis = create_genesis_config(1_000_000).genesis_config;
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis));
        let config = Config {
            mem_cap_mb,
            ..Config::default()
        };
        let c = Comparator::new(
            Arc::new(config),
            bank_forks,
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
        );
        (c, dir)
    }

    fn frame(slot: Slot, i: u64) -> AgaveFrame {
        let mut sig = [0u8; 64];
        sig[..8].copy_from_slice(&i.to_le_bytes());
        AgaveFrame {
            slot,
            bank_id: 1,
            signature: Signature::from(sig),
            accounts: vec![(Pubkey::new_unique(), AccountSharedData::new(1, 1000, &Pubkey::default()))],
            t: Instant::now(),
            t_unix_ns: 0,
        }
    }

    fn processed(slot: Slot, i: u64) -> Box<AgaveProcessed> {
        let mut sig = [0u8; 64];
        sig[..8].copy_from_slice(&i.to_le_bytes());
        Box::new(AgaveProcessed {
            slot,
            bank_id: 1,
            parent_slot: slot - 1,
            index: i as usize,
            signature: Signature::from(sig),
            message_hash: solana_hash::Hash::default(),
            result: Ok(ProcessedTransaction::NoOp(Box::new(NoOpTransaction {
                validation_error: TransactionError::AccountNotFound,
                fee_payer_balance: None,
                compute_unit_limit: 0,
                loaded_accounts_bytes_limit: 0,
            }))),
            balances: None,
            cost: None,
            t: Instant::now(),
        })
    }

    /// Agave frames and results of slots FL has no run for (and will not start one for) are
    /// dropped at once: the catch-up backlog after a restart must not accumulate.
    #[test]
    fn test_agave_side_of_slots_without_runs_is_dropped() {
        let (mut c, _dir) = comparator(4096);
        // No run yet since FL was enabled: everything is dropped.
        for i in 0..1_000 {
            c.accept_agave_frame(frame(100 + i % 50, i));
            c.accept_agave_processed(processed(100 + i % 50, i));
        }
        assert_eq!(c.agave.len(), 0);
        assert_eq!(c.full.agave_len(), 0);
        assert_eq!(c.interval.agave_only_skipped, 1_000);
        assert_eq!(c.full.interval.unjoined_agave, 1_000);
        // FL runs slot 500: the backlog below it is dropped, slot 500 and later slots are kept,
        // a skipped slot is dropped.
        c.slot_runs.insert(500, 1);
        c.first_run_slot = Some(500);
        c.max_run_slot = 500;
        c.skipped.insert(502, "parent_timeout");
        for (slot, kept) in [(400, false), (499, false), (500, true), (501, true), (502, false)] {
            c.accept_agave_frame(frame(slot, slot));
            c.accept_agave_processed(processed(slot, slot));
            let key = (slot, frame(slot, slot).signature);
            assert_eq!(c.agave.remove(&key).is_some(), kept, "frame of slot {slot}");
        }
        assert_eq!(c.full.agave_len(), 2);
    }

    /// The join maps are bounded by bytes (an eighth of the cap each), oldest evicted.
    #[test]
    fn test_join_maps_are_bounded() {
        let (mut c, _dir) = comparator(8); // 1 MiB per map
        c.slot_runs.insert(7, 1);
        c.first_run_slot = Some(7);
        c.max_run_slot = 7;
        for i in 0..10_000 {
            c.accept_agave_frame(frame(7, i));
        }
        assert!(c.agave.bytes() <= 1 << 20, "{}", c.agave.bytes());
        assert!(c.agave.len() < 1_000);
        assert!(c.interval.held_evicted > 9_000);
        c.release();
        assert_eq!((c.agave.len(), c.agave.bytes()), (0, 0));
    }
}
