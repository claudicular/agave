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
    solana_runtime::bank_forks::BankForks,
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
}

/// The coordinator's sink: forwards FINAL transactions to the comparator.
pub struct CmpSink {
    pub tx: Sender<CmpMsg>,
    pub drops: Arc<AtomicU64>,
}

impl FinalSink for CmpSink {
    fn on_final(&mut self, f: Finalized) {
        let Ok(outcome) = f.payload.downcast::<TxOutcome>() else {
            return;
        };
        let record = FinalRecord {
            run_id: f.run_id,
            outcome: *outcome,
            incarnations: f.incarnations,
            speculative: f.speculative,
            n_preds: f.n_preds,
            t_ingest: f.t_ingest,
            t_exec_start: f.t_exec_start,
            t_exec_end: f.t_exec_end,
            t_final: f.t_final,
            t_final_unix_ns: unix_ns(),
        };
        if let Err(TrySendError::Full(_)) = self.tx.try_send(CmpMsg::Final(Box::new(record))) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn on_run_end(&mut self, run_id: RunId, summary: RunSummary) {
        let _ = self.tx.try_send(CmpMsg::RunEnd { run_id, summary });
    }
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
    fl: HashMap<(Slot, Signature), FinalRecord>,
    agave: HashMap<(Slot, Signature), AgaveFrame>,
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
    totals: Interval,
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
                 incarnations,spec,exec_us,n_preds,kind,ok,parent_slot",
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
        Self {
            config,
            bank_forks,
            fl: HashMap::new(),
            agave: HashMap::new(),
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
            totals: Interval::default(),
        }
    }

    pub fn run_loop(
        &mut self,
        cmp_rx: Receiver<CmpMsg>,
        frame_rx: Receiver<AgaveFrame>,
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
                    Ok(frame) => self.on_agave(frame),
                    Err(_) => { self.flush(); return; }
                },
                default(Duration::from_millis(100)) => {},
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

    fn flush(&mut self) {
        for w in [&mut self.export, &mut self.mismatches, &mut self.summaries]
            .into_iter()
            .flatten()
        {
            w.flush();
        }
    }

    pub fn on_msg(&mut self, msg: CmpMsg) {
        match msg {
            CmpMsg::Final(record) => self.on_final(*record),
            CmpMsg::RunStart {
                run_id,
                run,
                parent_frozen_at_first_set,
                parent_wait_us,
                ctx_build_us: _,
            } => {
                self.interval.runs_started += 1;
                if parent_frozen_at_first_set {
                    self.interval.parent_frozen_at_first_set += 1;
                } else {
                    self.interval.parent_waited += 1;
                    self.interval.parent_wait_us.push(parent_wait_us as i64);
                }
                self.slot_runs.insert(run.slot, run_id);
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
        }
    }

    fn on_final(&mut self, record: FinalRecord) {
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
        } else {
            self.fl.insert(key, record);
        }
    }

    fn on_agave(&mut self, frame: AgaveFrame) {
        let key = (frame.slot, frame.signature);
        if let Some(record) = self.fl.remove(&key) {
            self.join(record, Some(frame));
        } else {
            self.agave.insert(key, frame);
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
            let line = format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:?},{},{}",
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
    /// FL wrote must equal the frozen bank's value (fee collector and incinerator excepted:
    /// agave writes them at freeze).
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
        let mut bad = 0u64;
        for key in info.run.overlay.written_keys().into_iter().take(10_000) {
            if key == collector || key == incinerator::id() {
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
        let now = Instant::now();
        let stale_fl: Vec<(Slot, Signature)> = self
            .fl
            .iter()
            .filter(|(_, r)| now.saturating_duration_since(r.t_final) > horizon)
            .map(|(k, _)| *k)
            .collect();
        for key in stale_fl {
            if let Some(record) = self.fl.remove(&key) {
                self.join(record, None);
            }
        }
        let stale_agave: Vec<(Slot, Signature)> = self
            .agave
            .iter()
            .filter(|(_, f)| now.saturating_duration_since(f.t) > horizon)
            .map(|(k, _)| *k)
            .collect();
        for key in stale_agave {
            if self.agave.remove(&key).is_some() {
                if self.slot_runs.contains_key(&key.0) {
                    self.interval.agave_only_ran += 1;
                } else {
                    self.interval.agave_only_skipped += 1;
                }
            }
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
        self.interval_start = Instant::now();
        for v in [
            &mut iv.lead_us,
            &mut iv.lead_us_token,
            &mut iv.fl_latency_us,
            &mut iv.fl_latency_us_token,
            &mut iv.agave_latency_us,
            &mut iv.agave_latency_us_token,
            &mut iv.parent_wait_us,
        ] {
            v.sort_unstable();
        }
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
             cmp_drops={}",
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
                 \"tap_drops\":{tap_drops},\"frame_drops\":{frame_drops}}}",
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
            );
            w.write_line(&line);
        }
        self.totals.matched += iv.matched;
        self.totals.mismatched += iv.mismatched;
        self.flush();
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
