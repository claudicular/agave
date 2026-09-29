//! Ingest: slot lifecycle, the position gate, blockstore and ring reads, sanitization and
//! run creation. Runs on `solFlIngest`.
//!
//! For each slot N FL releases entries strictly in ledger order through a [`Gate`] fed by
//! agave's completed data sets (blockstore) and, with `input = "dual"`, by the proxy's ring
//! v2 (streamed, position-tagged records), whichever delivers a position first. Once N's parent P is frozen (block-metadata tee), FL computes
//! N's environment from P, creates a run, sanitizes each released transaction exactly as
//! replay does (child-slot ALT resolution, lock validation, static checks) and hands them to
//! the coordinator in ledger order. Transaction ordinals count every transaction of the slot
//! from shred 0, which equals agave's `transaction_indexes`.
//!
//! With `chain = true`, a slot whose parent P is not frozen yet starts on top of FL's own
//! complete run of P (P's parent frozen, P's entries complete, no unprocessable
//! transaction, agave's bank P created): see [`Run::new_chained`]. Its transaction 0 is a
//! pseudo-transaction for P's freeze-time writes (and the child's SlotHashes), completed
//! here when agave freezes P.

use {
    crate::{
        compare::CmpMsg,
        config::Config,
        forks::FrozenBanks,
        gate::{Gate, Piece, Source},
        mv::TxIdx,
        program_cache::ProgramCaches,
        ring::{Poll, RingReader},
        run::Run,
        sched::{CoordMsg, ExecOutput, RunId},
        tap::TapBatch,
        tees::AgaveEvent,
    },
    crossbeam_channel::{Receiver, Sender, select},
    log::info,
    solana_clock::Slot,
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_ledger::blockstore::Blockstore,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    std::{
        collections::{BTreeMap, HashMap},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    },
};

pub struct IngestDeps {
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub blockstore: Arc<Blockstore>,
}

struct ReleasedSet {
    entries: Vec<Entry>,
    source: Source,
    t_tap: Instant,
    t_tap_unix_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotStatus {
    Collecting,
    Running,
    Complete,
    Skipped(&'static str),
}

struct SlotIngest {
    parent: Option<Slot>,
    first_seen: Instant,
    gate: Gate,
    released: Vec<ReleasedSet>,
    run: Option<(RunId, Arc<Run>)>,
    /// Kept after the run's input completes (the Arc is released then).
    run_id: Option<RunId>,
    next_ordinal: TxIdx,
    status: SlotStatus,
    parent_frozen_at_first_set: bool,
    input_complete: bool,
    /// Hash of the last released entry (the slot's last tick once input is complete).
    last_entry_hash: Option<Hash>,
    /// This slot's complete-input, non-chained run, kept while the slot is unfrozen so a
    /// child can chain on it.
    done_run: Option<Arc<Run>>,
    /// A chained run waiting for its parent's freeze.
    chain_run: Option<(RunId, Arc<Run>)>,
}

#[derive(Default, Debug, Clone)]
pub struct IngestStats {
    pub data_sets: u64,
    pub ring_records: u64,
    pub ring_resets: u64,
    pub ring_decode_errors: u64,
    pub blockstore_reads_skipped: u64,
    pub duplicate_sets: u64,
    pub blockstore_read_us: u64,
    pub sanitize_us: u64,
    pub txs: u64,
    pub runs_started: u64,
    pub runs_chained: u64,
    pub chains_resolved: u64,
    pub slots_skipped: HashMap<&'static str, u64>,
}

pub struct Ingest {
    deps: IngestDeps,
    config: Arc<Config>,
    coord_tx: Sender<CoordMsg>,
    cmp_tx: Sender<CmpMsg>,
    frozen: FrozenBanks,
    programs: ProgramCaches,
    slots: BTreeMap<Slot, SlotIngest>,
    dead: BTreeMap<Slot, ()>,
    next_run_id: RunId,
    readonly_owners: Arc<Vec<Pubkey>>,
    root: Slot,
    ring: Option<RingReader>,
    ring_retry_at: Instant,
    /// Runs report completion here (their slot).
    complete_tx: Sender<Slot>,
    complete_rx: Receiver<Slot>,
    /// Everything was dropped because the fast lane is off (re-seeded when it comes back).
    released: bool,
    last_bank_prune: Instant,
    last_program_stats: Instant,
    pub stats: IngestStats,
    /// Memory-cap trip handling (see `maybe_auto_reenable`).
    cap_tripped_at: Option<Instant>,
    caught_up_since: Option<Instant>,
    last_reenable: Option<Instant>,
    reenable_backoff: Duration,
}

/// The node counts as caught up when replay's working bank is within this many slots of the
/// blockstore's newest slot.
const CAUGHT_UP_SLOTS: u64 = 8;

impl Ingest {
    pub fn new(
        deps: IngestDeps,
        config: Arc<Config>,
        coord_tx: Sender<CoordMsg>,
        cmp_tx: Sender<CmpMsg>,
        readonly_owners: Arc<Vec<Pubkey>>,
    ) -> Self {
        let fork_graph = Arc::new(RwLock::new(crate::forks::FlForkGraph::default()));
        let (complete_tx, complete_rx) = crossbeam_channel::bounded(1024);
        let mut ingest = Self {
            deps,
            config,
            coord_tx,
            cmp_tx,
            frozen: FrozenBanks::default(),
            programs: ProgramCaches::new(fork_graph),
            slots: BTreeMap::new(),
            dead: BTreeMap::new(),
            next_run_id: 1,
            readonly_owners,
            root: 0,
            ring: None,
            ring_retry_at: Instant::now(),
            complete_tx,
            complete_rx,
            released: false,
            last_bank_prune: Instant::now(),
            last_program_stats: Instant::now(),
            stats: IngestStats::default(),
            cap_tripped_at: None,
            caught_up_since: None,
            last_reenable: None,
            reenable_backoff: Duration::from_secs(30),
        };
        ingest.seed();
        ingest
    }

    fn seed(&mut self) {
        let root = self
            .deps
            .bank_forks
            .read()
            .map(|forks| forks.root())
            .unwrap_or(0);
        for bank in self.frozen.seed(&self.deps.bank_forks) {
            if let Ok(mut graph) = self.programs.fork_graph().write() {
                graph.set_parent(bank.slot(), bank.parent_slot());
            }
        }
        self.set_root(root);
        info!(
            "fast lane: ingest seeded with {} frozen banks, root {root}",
            self.frozen.len()
        );
    }

    fn set_root(&mut self, root: Slot) {
        if root <= self.root {
            return;
        }
        self.root = root;
        self.programs.set_root(root);
        self.frozen.prune(root, 512);
        let keep = self.slots.split_off(&root);
        for (slot, state) in std::mem::replace(&mut self.slots, keep) {
            if let Some((run_id, _)) = state.run {
                if !matches!(state.status, SlotStatus::Complete | SlotStatus::Skipped(_)) {
                    let _ = self.coord_tx.send(CoordMsg::AbortRun {
                        run_id,
                        reason: "below_root",
                    });
                    self.programs.prune_slot(slot);
                }
            }
            if let Some((run_id, _)) = state.chain_run {
                let _ = self.coord_tx.send(CoordMsg::AbortRun {
                    run_id,
                    reason: "below_root",
                });
            }
        }
        self.dead = self.dead.split_off(&root.saturating_sub(64));
    }

    /// Main loop.
    pub fn run_loop(
        &mut self,
        tap_rx: Receiver<TapBatch>,
        event_rx: Receiver<AgaveEvent>,
        exit: Arc<AtomicBool>,
    ) {
        let mut last_housekeeping = Instant::now();
        let spin = Duration::from_micros(self.config.ingest_spin_us);
        let mut idle_since = Instant::now();
        loop {
            if exit.load(Ordering::Relaxed) {
                return;
            }
            if spin.is_zero() || idle_since.elapsed() >= spin && spin < crate::sched::SPIN_FOREVER
            {
                select! {
                    recv(tap_rx) -> msg => match msg {
                        Ok(batch) => self.on_tap(batch),
                        Err(_) => return,
                    },
                    recv(event_rx) -> msg => match msg {
                        Ok(event) => self.on_event(event),
                        Err(_) => return,
                    },
                    recv(self.complete_rx) -> msg => {
                        if let Ok(slot) = msg {
                            self.on_run_complete(slot);
                        }
                    },
                    default(Duration::from_millis(20)) => {},
                }
                idle_since = Instant::now();
            } else {
                // Busy-poll (dedicated core): the ring first (earliest input), then taps.
                let mut worked = !self.released && self.poll_ring();
                match tap_rx.try_recv() {
                    Ok(batch) => {
                        self.on_tap(batch);
                        worked = true;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => return,
                    Err(crossbeam_channel::TryRecvError::Empty) => {}
                }
                match event_rx.try_recv() {
                    Ok(event) => {
                        self.on_event(event);
                        worked = true;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => return,
                    Err(crossbeam_channel::TryRecvError::Empty) => {}
                }
                if let Ok(slot) = self.complete_rx.try_recv() {
                    self.on_run_complete(slot);
                    worked = true;
                }
                if worked {
                    idle_since = Instant::now();
                } else {
                    std::hint::spin_loop();
                    if last_housekeeping.elapsed() < Duration::from_millis(20) {
                        continue;
                    }
                }
            }
            self.check_active();
            if last_housekeeping.elapsed() >= Duration::from_millis(20) {
                self.housekeeping();
                last_housekeeping = Instant::now();
            }
        }
    }

    fn skip(&mut self, slot: Slot, reason: &'static str) {
        if let Some(state) = self.slots.get_mut(&slot) {
            if matches!(state.status, SlotStatus::Skipped(_)) {
                return;
            }
            state.chain_run = None;
            state.done_run = None;
            if state.status == SlotStatus::Complete {
                // Input was complete but the coordinator may still be executing it.
                if let Some(run_id) = state.run_id {
                    let _ = self.coord_tx.send(CoordMsg::AbortRun { run_id, reason });
                }
                state.status = SlotStatus::Skipped(reason);
                *self.stats.slots_skipped.entry(reason).or_default() += 1;
                let _ = self.cmp_tx.try_send(CmpMsg::SlotSkipped { slot, reason });
                return;
            }
            state.status = SlotStatus::Skipped(reason);
            state.released.clear();
            if let Some((run_id, _)) = state.run.take() {
                let _ = self.coord_tx.send(CoordMsg::AbortRun { run_id, reason });
                self.programs.prune_slot(slot);
            }
        }
        *self.stats.slots_skipped.entry(reason).or_default() += 1;
        let _ = self.cmp_tx.try_send(CmpMsg::SlotSkipped { slot, reason });
    }

    /// With the fast lane off, drop everything it holds (runs, banks, program cache, slot
    /// state); when it comes back, re-seed from bank_forks.
    fn check_active(&mut self) {
        if crate::control::is_active() {
            if self.released {
                self.released = false;
                self.seed();
                info!("fast lane: re-enabled, ingest re-seeded");
            }
            return;
        }
        if !self.released {
            self.release("disabled");
        }
    }

    fn release(&mut self, reason: &'static str) {
        for (slot, state) in std::mem::take(&mut self.slots) {
            if let Some(run_id) = state.run_id {
                let _ = self.coord_tx.send(CoordMsg::AbortRun { run_id, reason });
            }
            if !matches!(state.status, SlotStatus::Skipped(_)) {
                *self.stats.slots_skipped.entry(reason).or_default() += 1;
                let _ = self.cmp_tx.try_send(CmpMsg::SlotSkipped { slot, reason });
            }
        }
        self.frozen = FrozenBanks::default();
        self.dead.clear();
        self.programs.reset();
        self.ring = None;
        crate::mem::BANKS_HELD.set(0);
        crate::mem::INGEST_PENDING_BYTES.set(0);
        crate::mem::PROGRAM_ENTRIES.set(0);
        crate::mem::PROGRAM_BYTES.set(0);
        self.released = true;
        crate::control::reset_run_slots();
        info!("fast lane: released all ingest state ({reason})");
    }

    /// Hard memory cap: over `mem_cap_mb` of FL-held bytes, disable the fast lane (the next
    /// loop iteration releases everything). Stays off until `enable` in the control file.
    fn check_mem_cap(&mut self) {
        let snapshot = crate::mem::Snapshot::now();
        let cap = (self.config.mem_cap_mb as i64).saturating_mul(1 << 20);
        let over = snapshot.total_bytes() > cap || snapshot.live_runs > crate::mem::MAX_LIVE_RUNS;
        if over && crate::control::is_active() {
            crate::mem::CAP_TRIPS.fetch_add(1, Ordering::Relaxed);
            log::error!(
                "fast lane: holding {} MiB (cap {} MiB), {} live runs (cap {}); disabling and \
                 releasing (re-enabled automatically once under {}% of the cap and caught up): \
                 {:?}",
                snapshot.total_bytes() >> 20,
                self.config.mem_cap_mb,
                snapshot.live_runs,
                crate::mem::MAX_LIVE_RUNS,
                crate::mem::CAP_LOW_WATER_PCT,
                snapshot
            );
            // Repeated trips back off: 30 s, doubling up to 10 min while trips keep coming.
            let now = Instant::now();
            self.reenable_backoff = match self.last_reenable {
                Some(t) if now.duration_since(t) < Duration::from_secs(600) => {
                    (self.reenable_backoff * 2).min(Duration::from_secs(600))
                }
                _ => Duration::from_secs(30),
            };
            self.cap_tripped_at = Some(now);
            self.caught_up_since = None;
            crate::control::disable_for_cap();
        }
    }

    /// After a cap trip: re-enable once memory is under the low watermark, the node has been
    /// caught up (replay within a few slots of the blockstore's newest slot) for 5 s and the
    /// backoff has passed. Never after a poison or an operator's command.
    fn maybe_auto_reenable(&mut self) {
        if !crate::control::cap_disabled() || crate::control::is_poisoned() {
            self.caught_up_since = None;
            return;
        }
        let cap = (self.config.mem_cap_mb as i64).saturating_mul(1 << 20);
        let low = cap / 100 * crate::mem::CAP_LOW_WATER_PCT;
        let now = Instant::now();
        let caught_up = {
            let working = self
                .deps
                .bank_forks
                .read()
                .ok()
                .map(|forks| forks.working_bank().slot());
            let highest = self.deps.blockstore.highest_slot().ok().flatten();
            matches!((working, highest), (Some(w), Some(h)) if w + CAUGHT_UP_SLOTS >= h)
        };
        if crate::mem::Snapshot::now().total_bytes() > low || !caught_up {
            self.caught_up_since = None;
            return;
        }
        let since = *self.caught_up_since.get_or_insert(now);
        let backoff_over = self
            .cap_tripped_at
            .is_none_or(|t| now.duration_since(t) >= self.reenable_backoff);
        if now.duration_since(since) < Duration::from_secs(5) || !backoff_over {
            return;
        }
        if crate::control::auto_reenable() {
            crate::mem::CAP_REENABLES.fetch_add(1, Ordering::Relaxed);
            self.last_reenable = Some(now);
            self.caught_up_since = None;
            log::warn!(
                "fast lane: re-enabled after a memory-cap trip (memory under {}% of the cap, \
                 node caught up; next backoff {:?})",
                crate::mem::CAP_LOW_WATER_PCT,
                self.reenable_backoff
            );
        }
    }

    fn update_gauges(&mut self) {
        let pending: i64 = self
            .slots
            .values()
            .flat_map(|s| s.released.iter())
            .flat_map(|set| set.entries.iter())
            .map(entry_bytes)
            .sum();
        crate::mem::INGEST_PENDING_BYTES.set(pending);
        if self.last_bank_prune.elapsed() >= Duration::from_millis(250) {
            self.last_bank_prune = Instant::now();
            self.frozen
                .retain_live(&self.deps.bank_forks, self.root, crate::mem::MAX_BANKS_HELD);
            crate::mem::BANKS_HELD.set(self.frozen.len() as i64);
        }
        if self.last_program_stats.elapsed() >= Duration::from_secs(1) {
            self.last_program_stats = Instant::now();
            let (entries, bytes) = self.programs.loaded_stats();
            crate::mem::PROGRAM_ENTRIES.set(entries as i64);
            crate::mem::PROGRAM_BYTES.set(bytes as i64);
        }
    }

    fn housekeeping(&mut self) {
        // Root from bank_forks: agave's rooted notifications do not reach the slot-status tee
        // (the geyser plugin service's own observer thread sends them), so FL cannot rely
        // on `AgaveEvent::Rooted` to prune.
        let root = self.deps.bank_forks.read().ok().map(|forks| forks.root());
        if let Some(root) = root {
            self.set_root(root);
        }
        self.maybe_auto_reenable();
        if self.released {
            return;
        }
        self.update_gauges();
        self.check_mem_cap();
        let parent_wait = Duration::from_millis(self.config.parent_wait_ms);
        let timed_out: Vec<Slot> = self
            .slots
            .iter()
            .filter(|(_, s)| {
                s.status == SlotStatus::Collecting
                    && s.run.is_none()
                    && s.first_seen.elapsed() > parent_wait
            })
            .map(|(slot, _)| *slot)
            .collect();
        for slot in timed_out {
            // Retry once more before giving up (the freeze event may have been dropped).
            self.try_start_run(slot);
            if self.slots.get(&slot).is_some_and(|s| s.run.is_none()) {
                self.skip(slot, "parent_timeout");
            }
        }
        // Chained runs whose parent froze without an event reaching us (tee drop).
        let pending: Vec<Slot> = self
            .slots
            .iter()
            .filter(|(_, s)| s.chain_run.is_some())
            .map(|(slot, _)| *slot)
            .collect();
        for slot in pending {
            self.resolve_chain(slot);
        }
        // Forget old slot state (bounded memory even if root stalls).
        let old: Vec<Slot> = self
            .slots
            .iter()
            .filter(|(_, s)| s.first_seen.elapsed() > Duration::from_secs(30))
            .map(|(slot, _)| *slot)
            .collect();
        for slot in old {
            if let Some(state) = self.slots.remove(&slot) {
                if let Some((run_id, _)) = state.run {
                    if state.status != SlotStatus::Complete {
                        let _ = self.coord_tx.send(CoordMsg::AbortRun {
                            run_id,
                            reason: "stale",
                        });
                    }
                }
                if let Some((run_id, _)) = state.chain_run {
                    let _ = self.coord_tx.send(CoordMsg::AbortRun {
                        run_id,
                        reason: "chain_stale",
                    });
                }
            }
        }
    }

    fn on_event(&mut self, event: AgaveEvent) {
        match event {
            AgaveEvent::Frozen {
                slot,
                bank_id,
                parent_slot,
                t,
            } => {
                if let Ok(mut graph) = self.programs.fork_graph().write() {
                    graph.set_parent(slot, parent_slot);
                }
                let _ = self
                    .frozen
                    .on_frozen(&self.deps.bank_forks, slot, Some(bank_id));
                let _ = self.cmp_tx.try_send(CmpMsg::SlotFrozen { slot, bank_id, t });
                if let Some(state) = self.slots.get_mut(&slot) {
                    state.done_run = None;
                }
                let chained: Vec<Slot> = self
                    .slots
                    .iter()
                    .filter(|(_, s)| s.parent == Some(slot) && s.chain_run.is_some())
                    .map(|(child, _)| *child)
                    .collect();
                for child in chained {
                    self.resolve_chain(child);
                }
                let children: Vec<Slot> = self
                    .slots
                    .iter()
                    .filter(|(_, s)| {
                        s.parent == Some(slot) && s.run.is_none() && s.run_id.is_none()
                    })
                    .map(|(child, _)| *child)
                    .collect();
                for child in children {
                    self.try_start_run(child);
                }
            }
            AgaveEvent::Dead { slot } => {
                self.dead.insert(slot, ());
                self.frozen.remove(slot);
                if self.slots.contains_key(&slot) {
                    self.skip(slot, "dead");
                }
                let children: Vec<Slot> = self
                    .slots
                    .iter()
                    .filter(|(_, s)| s.parent == Some(slot))
                    .map(|(child, _)| *child)
                    .collect();
                for child in children {
                    self.skip(child, "parent_dead");
                }
            }
            AgaveEvent::Rooted { slot } => {
                self.set_root(slot);
                let _ = self.cmp_tx.try_send(CmpMsg::Rooted { slot });
            }
            AgaveEvent::Created { .. } => {}
        }
    }

    /// State of `slot`, created on first sight with its parent.
    fn slot_state(&mut self, slot: Slot, parent: Option<Slot>, t: Instant) -> Option<&mut SlotIngest> {
        if slot <= self.root || self.dead.contains_key(&slot) {
            return None;
        }
        if !self.slots.contains_key(&slot) {
            let parent = parent.or_else(|| {
                self.deps
                    .blockstore
                    .meta(slot)
                    .ok()
                    .flatten()
                    .and_then(|meta| meta.parent_slot)
            });
            let parent_frozen = parent.is_some_and(|p| self.frozen.get(p).is_some());
            self.slots.insert(
                slot,
                SlotIngest {
                    parent,
                    first_seen: t,
                    gate: Gate::new(),
                    released: Vec::new(),
                    run: None,
                    run_id: None,
                    next_ordinal: 0,
                    status: SlotStatus::Collecting,
                    parent_frozen_at_first_set: parent_frozen,
                    input_complete: false,
                    last_entry_hash: None,
                    done_run: None,
                    chain_run: None,
                },
            );
        }
        let state = self.slots.get_mut(&slot)?;
        if state.parent.is_none() {
            state.parent = parent;
        }
        match state.status {
            // A complete slot still accepts pieces for the cross-check (nothing is released).
            SlotStatus::Skipped(_) => None,
            _ => Some(state),
        }
    }

    /// Feed a piece to the slot's gate; returns false if the sources disagreed.
    fn push_piece(&mut self, slot: Slot, piece: Piece) -> bool {
        let Some(state) = self.slots.get_mut(&slot) else {
            return true;
        };
        for released in state.gate.push(piece) {
            if let Some(last) = released.entries.last() {
                state.last_entry_hash = Some(last.hash);
            }
            state.released.push(ReleasedSet {
                entries: released.entries,
                source: released.source,
                t_tap: released.t,
                t_tap_unix_ns: released.t_unix_ns,
            });
        }
        if state.gate.is_complete() {
            state.input_complete = true;
        }
        !state.gate.mismatch
    }

    fn on_tap(&mut self, batch: TapBatch) {
        let mut touched = Vec::new();
        for set in &batch.sets {
            let slot = set.slot;
            let check = self.config.ring_blockstore_check;
            let Some(state) = self.slot_state(slot, None, batch.t) else {
                continue;
            };
            let start = set.indices.start;
            let already = state.gate.batch_completed(start);
            let behind = start < state.gate.frontier().0;
            self.stats.data_sets += 1;
            if already && !check {
                // The ring already delivered this batch; the comparator checks the outcome.
                self.stats.blockstore_reads_skipped += 1;
                continue;
            }
            if behind && !check {
                self.stats.duplicate_sets += 1;
                continue;
            }
            let t0 = Instant::now();
            let entries = match self.deps.blockstore.get_entries_in_data_block(
                slot,
                set.indices.clone(),
                None,
            ) {
                Ok(entries) => entries,
                Err(_) => {
                    self.skip(slot, "blockstore_read");
                    continue;
                }
            };
            self.stats.blockstore_read_us += t0.elapsed().as_micros() as u64;
            let last_index = self
                .deps
                .blockstore
                .meta(slot)
                .ok()
                .flatten()
                .and_then(|meta| meta.last_index);
            let end = set.indices.end.saturating_sub(1);
            let piece = Piece {
                batch_start: start,
                entry_offset: 0,
                entries,
                final_end: Some(end),
                last_in_slot: last_index == Some(u64::from(end)),
                source: Source::Blockstore,
                t: batch.t,
                t_unix_ns: batch.t_unix_ns,
            };
            if !self.push_piece(slot, piece) {
                self.skip(slot, "source_mismatch");
                continue;
            }
            if let (Some(state), Some(last_index)) = (self.slots.get_mut(&slot), last_index) {
                state.gate.set_complete_if_past(last_index);
                if state.gate.is_complete() {
                    state.input_complete = true;
                }
            }
            if !touched.contains(&slot) {
                touched.push(slot);
            }
        }
        for slot in touched {
            self.try_start_run(slot);
        }
    }

    /// Drain up to a bounded number of ring records (phase-2 input).
    fn poll_ring(&mut self) -> bool {
        if !self.config.input_dual {
            return false;
        }
        let Some(path) = self.config.ring_path.clone() else {
            return false;
        };
        if self.ring.as_ref().is_some_and(|r| r.stale()) {
            self.ring = None;
        }
        if self.ring.is_none() {
            if Instant::now() < self.ring_retry_at {
                return false;
            }
            match RingReader::open(&path) {
                Ok(reader) => {
                    info!("fast lane: reading ring v2 at {}", path.display());
                    self.ring = Some(reader);
                }
                Err(_) => {
                    self.ring_retry_at = Instant::now() + Duration::from_secs(1);
                    return false;
                }
            }
        }
        let mut worked = false;
        let mut touched = Vec::new();
        for _ in 0..64 {
            let Some(reader) = self.ring.as_mut() else {
                break;
            };
            let (meta, payload) = match reader.poll() {
                Poll::Record(meta, payload) => (meta, payload),
                Poll::Empty => break,
                Poll::Reset => {
                    self.stats.ring_resets += 1;
                    break;
                }
            };
            worked = true;
            self.stats.ring_records += 1;
            let t = Instant::now();
            let slot = meta.slot;
            if self.slot_state(slot, meta.parent(), t).is_none() {
                continue;
            }
            let entries: Vec<Entry> = if payload.is_empty() {
                Vec::new()
            } else {
                match wincode::deserialize::<solana_entry::block_component::BlockComponent>(
                    &payload,
                ) {
                    Ok(solana_entry::block_component::BlockComponent::EntryBatch(entries)) => {
                        entries
                    }
                    Ok(_) => Vec::new(),
                    Err(_) => {
                        self.stats.ring_decode_errors += 1;
                        continue;
                    }
                }
            };
            let piece = Piece {
                batch_start: meta.batch_start,
                entry_offset: meta.entry_offset,
                entries,
                final_end: meta.is_final().then_some(meta.batch_end),
                last_in_slot: meta.last_in_slot(),
                source: Source::Ring,
                t,
                t_unix_ns: meta.t_publish_ns,
            };
            if !self.push_piece(slot, piece) {
                self.skip(slot, "source_mismatch");
                continue;
            }
            if !touched.contains(&slot) {
                touched.push(slot);
            }
        }
        for slot in touched {
            self.try_start_run(slot);
        }
        worked
    }

    fn active_runs(&self) -> usize {
        self.slots
            .values()
            .filter(|s| s.status == SlotStatus::Running)
            .count()
    }

    #[allow(dead_code)]
    fn overlay_bytes(&self) -> usize {
        self.slots
            .values()
            .filter_map(|s| s.run.as_ref())
            .map(|(_, run)| run.overlay.bytes())
            .sum()
    }

    fn try_start_run(&mut self, slot: Slot) {
        let Some(state) = self.slots.get(&slot) else {
            return;
        };
        match state.status {
            SlotStatus::Running => {
                self.feed(slot);
                return;
            }
            SlotStatus::Collecting => {}
            SlotStatus::Complete | SlotStatus::Skipped(_) => return,
        }
        let Some(parent_slot) = state.parent else {
            return;
        };
        if self.dead.contains_key(&parent_slot) {
            self.skip(slot, "parent_dead");
            return;
        }
        let frozen_parent = self.frozen.get(parent_slot).cloned();
        let chain = match frozen_parent {
            Some(_) => None,
            None if self.config.chain => match self.chain_parent(parent_slot) {
                Some(chain) => Some(chain),
                None => return,
            },
            None => return,
        };
        if self.active_runs() >= self.config.max_runs {
            return;
        }
        // Soft cap: no new run while live overlays hold more than half the hard cap.
        if crate::mem::OVERLAY_BYTES.get() as usize > self.config.mem_cap_mb.saturating_mul(1 << 19) {
            self.skip(slot, "mem_cap");
            return;
        }
        let created = match (frozen_parent, chain) {
            (Some(parent), _) => self.create_run(slot, &parent),
            (None, Some((parent_run, parent_bank, last_blockhash))) => {
                self.create_chained_run(slot, &parent_run, &parent_bank, last_blockhash)
            }
            (None, None) => return,
        };
        match created {
            Ok(()) => self.feed(slot),
            Err(reason) => self.skip(slot, reason),
        }
    }

    /// FL's run of the unfrozen `parent_slot` a child can chain on, with agave's bank for
    /// it and its last blockhash; `None` while not (yet) possible.
    fn chain_parent(&self, parent_slot: Slot) -> Option<(Arc<Run>, Arc<Bank>, Hash)> {
        let state = self.slots.get(&parent_slot)?;
        if state.status != SlotStatus::Complete || !state.input_complete {
            return None;
        }
        let parent_run = state.done_run.clone()?;
        if !parent_run.complete.load(Ordering::SeqCst)
            || !parent_run.complete_ok.load(Ordering::SeqCst)
            || parent_run.chain.is_some()
        {
            return None;
        }
        let last_blockhash = state.last_entry_hash?;
        let parent_bank = self.deps.bank_forks.read().ok()?.get(parent_slot)?;
        if parent_bank.is_frozen() {
            // The freeze event is on its way: run over the frozen bank instead.
            return None;
        }
        // Same fork version of P as FL's run (duplicate-block safety).
        let grandparent = parent_bank.parent()?;
        if grandparent.slot() != parent_run.parent_slot
            || grandparent.bank_id() != parent_run.parent_bank_id
        {
            return None;
        }
        Some((parent_run, parent_bank, last_blockhash))
    }

    /// A run of `slot` became complete (every transaction FINAL): children may chain on it.
    fn on_run_complete(&mut self, slot: Slot) {
        if !self.config.chain {
            return;
        }
        let children: Vec<Slot> = self
            .slots
            .iter()
            .filter(|(_, s)| {
                s.parent == Some(slot)
                    && s.status == SlotStatus::Collecting
                    && s.run_id.is_none()
            })
            .map(|(child, _)| *child)
            .collect();
        for child in children {
            self.try_start_run(child);
        }
    }

    /// Complete a chained run's parent-freeze pseudo-transaction once agave froze the
    /// parent (the same bank the run was built on).
    fn resolve_chain(&mut self, slot: Slot) {
        let Some((run_id, run)) = self.slots.get(&slot).and_then(|s| s.chain_run.clone()) else {
            return;
        };
        let Some(chain) = run.chain.as_ref() else {
            return;
        };
        let parent_slot = chain.parent_run.slot;
        let frozen = self.frozen.get(parent_slot).cloned().or_else(|| {
            self.deps
                .bank_forks
                .read()
                .ok()?
                .get(parent_slot)
                .filter(|bank| bank.is_frozen())
        });
        let Some(frozen) = frozen else {
            return;
        };
        if frozen.bank_id() != chain.parent_bank.bank_id() {
            self.skip(slot, "chain_parent_replaced");
            return;
        }
        let t0 = Instant::now();
        let Some(writes) = run.resolve_parent_freeze(&frozen) else {
            self.skip(slot, "chain_resolve");
            return;
        };
        run.overlay.install(0, 0, &writes, &[]);
        let out = ExecOutput {
            reads: Vec::new(),
            writes,
            payload: Box::new(()),
            unprocessable: false,
            exec_start: t0,
            exec_end: Instant::now(),
        };
        if let Some(state) = self.slots.get_mut(&slot) {
            state.chain_run = None;
        }
        self.stats.chains_resolved += 1;
        let _ = self.coord_tx.send(CoordMsg::ExternalDone { run_id, k: 0, out });
    }

    fn create_run(&mut self, slot: Slot, parent: &Arc<Bank>) -> Result<(), &'static str> {
        let t0 = Instant::now();
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        let mut run = Run::new(
            run_id,
            slot,
            parent,
            &mut self.programs,
            Arc::clone(&self.readonly_owners),
        )
        .map_err(unsupported_reason)?;
        if self.config.chain {
            run.complete_notify = Some(self.complete_tx.clone());
        }
        self.start_run(slot, run_id, Arc::new(run), t0)
    }

    fn create_chained_run(
        &mut self,
        slot: Slot,
        parent_run: &Arc<Run>,
        parent_bank: &Arc<Bank>,
        last_blockhash: Hash,
    ) -> Result<(), &'static str> {
        let t0 = Instant::now();
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        let run = Run::new_chained(
            run_id,
            slot,
            parent_run,
            parent_bank,
            last_blockhash,
            &mut self.programs,
            Arc::clone(&self.readonly_owners),
        )
        .map_err(unsupported_reason)?;
        self.stats.runs_chained += 1;
        self.start_run(slot, run_id, Arc::new(run), t0)
    }

    fn start_run(
        &mut self,
        slot: Slot,
        run_id: RunId,
        run: Arc<Run>,
        t0: Instant,
    ) -> Result<(), &'static str> {
        let Some(state) = self.slots.get_mut(&slot) else {
            return Err("gone");
        };
        state.run = Some((run_id, Arc::clone(&run)));
        state.run_id = Some(run_id);
        state.status = SlotStatus::Running;
        crate::control::note_run_slot(slot);
        state.next_ordinal = run.ordinal_base;
        if run.chain.is_some() {
            state.chain_run = Some((run_id, Arc::clone(&run)));
        }
        let parent_wait_us = state.first_seen.elapsed().as_micros() as u64;
        let parent_frozen_at_first_set = state.parent_frozen_at_first_set;
        self.stats.runs_started += 1;
        self.coord_tx
            .send(CoordMsg::NewRun {
                run_id,
                run: run.clone(),
            })
            .map_err(|_| "coordinator_gone")?;
        if crate::control::committing() {
            // Commit mode: the committer binds the run to agave's bank of the slot.
            let _ = self.coord_tx.send(CoordMsg::Sink(Box::new(
                crate::commit::CommitEvent::RunBegin {
                    run_id,
                    run: run.clone(),
                },
            )));
        }
        if let Some(meta) = run.provisional_meta() {
            self.coord_tx
                .send(CoordMsg::Txs {
                    run_id,
                    first: 0,
                    metas: vec![meta],
                    t_ingest: Instant::now(),
                })
                .map_err(|_| "coordinator_gone")?;
        }
        let _ = self.cmp_tx.try_send(CmpMsg::RunStart {
            run_id,
            run,
            parent_frozen_at_first_set,
            parent_wait_us,
            ctx_build_us: t0.elapsed().as_micros() as u64,
        });
        Ok(())
    }

    /// Sanitize released data sets into the run and hand them to the coordinator.
    fn feed(&mut self, slot: Slot) {
        let Some(state) = self.slots.get_mut(&slot) else {
            return;
        };
        let Some((run_id, run)) = state.run.clone() else {
            return;
        };
        let released = std::mem::take(&mut state.released);
        let mut ordinal = state.next_ordinal;
        let mut failure = None;
        'sets: for set in released {
            // Stream entry by entry: a set's first transactions reach the coordinator
            // without waiting for the whole set to be sanitized.
            for entry in set.entries {
                if entry.transactions.is_empty() {
                    continue;
                }
                let t0 = Instant::now();
                let first = ordinal;
                let (metas, entry_failure) = run.push_entries(
                    vec![entry],
                    set.t_tap,
                    set.t_tap_unix_ns,
                    set.source == Source::Ring,
                );
                self.stats.sanitize_us += t0.elapsed().as_micros() as u64;
                self.stats.txs += metas.len() as u64;
                ordinal += metas.len() as TxIdx;
                if !metas.is_empty() {
                    let _ = self.coord_tx.send(CoordMsg::Txs {
                        run_id,
                        first,
                        metas,
                        t_ingest: Instant::now(),
                    });
                }
                if entry_failure.is_some() {
                    failure = entry_failure;
                    break 'sets;
                }
            }
        }
        if let Some(reason) = failure {
            self.skip(slot, reason);
            return;
        }
        let Some(state) = self.slots.get_mut(&slot) else {
            return;
        };
        state.next_ordinal = ordinal;
        if state.input_complete && state.released.is_empty() {
            state.status = SlotStatus::Complete;
            // The coordinator and comparator hold the run from here on (and, for chaining,
            // `done_run` until the slot freezes).
            state.run = None;
            if self.config.chain && run.chain.is_none() {
                state.done_run = Some(Arc::clone(&run));
            }
            let _ = self.coord_tx.send(CoordMsg::InputComplete {
                run_id,
                total: ordinal,
            });
        }
    }
}

/// Approximate heap bytes of a decoded entry.
fn entry_bytes(entry: &Entry) -> i64 {
    48 + entry
        .transactions
        .iter()
        .map(|tx| {
            let message = &tx.message;
            64 + 64 * tx.signatures.len() as i64
                + 32 * message.static_account_keys().len() as i64
                + message
                    .instructions()
                    .iter()
                    .map(|ix| 24 + ix.data.len() as i64 + ix.accounts.len() as i64)
                    .sum::<i64>()
        })
        .sum::<i64>()
}

fn unsupported_reason(err: solana_runtime::bank::fast_lane::FastLaneUnsupported) -> &'static str {
    use solana_runtime::bank::fast_lane::FastLaneUnsupported as U;
    match err {
        U::ParentNotFrozen => "parent_not_frozen",
        U::UnknownFeeCollector => "unknown_fee_collector",
        U::ChildNotAfterParent => "child_not_after_parent",
        U::Alpenglow => "alpenglow",
        U::AlpenglowMigration => "alpenglow_migration",
        U::EpochBoundary => "epoch_boundary",
        U::LastSlotOfEpoch => "last_slot_of_epoch",
        U::RewardDistribution => "reward_distribution",
        U::LastRestartSlotChange => "last_restart_slot",
        U::BadSysvar => "bad_sysvar",
        U::UnknownBuiltin => "unknown_builtin",
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::ring::{self, test_writer::TestWriter},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::{Blockstore, CompletedDataSetInfo},
            get_tmp_ledger_path_auto_delete,
            shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
        },
        solana_message::Message,
        solana_runtime::{
            bank::SlotLeader,
            genesis_utils::{GenesisConfigInfo, create_genesis_config_with_leader},
        },
        solana_signature::Signature,
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::{Transaction, versioned::VersionedTransaction},
    };

    struct Fixture {
        _ledger: tempfile::TempDir,
        blockstore: Arc<Blockstore>,
        bank_forks: Arc<RwLock<BankForks>>,
        /// Three batches of entries for slot 2 (parent 1), with their shred ranges.
        batches: Vec<(Vec<Entry>, u32, u32)>,
        sigs: Vec<Signature>,
        /// The genesis validator (a leader whose vote account is staked).
        leader: SlotLeader,
        mint_keypair: Keypair,
    }

    fn fixture() -> Fixture {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            voting_keypair,
            validator_pubkey,
        } = create_genesis_config_with_leader(
            1_000_000_000_000,
            &Pubkey::new_unique(),
            1_000_000_000,
        );
        let (bank0, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let parent =
            Bank::new_from_parent_with_bank_forks(&bank_forks, bank0, SlotLeader::default(), 1);
        parent.freeze();
        let blockhash = parent.last_blockhash();
        let ledger = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger.path()).unwrap());
        let shredder = Shredder::new(2, 1, 0, 0).unwrap();
        let keypair = Keypair::new();
        let mut next_index = 0u32;
        let mut batches = Vec::new();
        let mut sigs = Vec::new();
        let mut n = 0u64;
        for b in 0..3 {
            let entries: Vec<Entry> = (0..4)
                .map(|_| {
                    let txs = (0..2)
                        .map(|_| {
                            n += 1;
                            let to = Pubkey::new_unique();
                            let tx = Transaction::new(
                                &[&mint_keypair],
                                Message::new(
                                    &[system_instruction::transfer(
                                        &mint_keypair.pubkey(),
                                        &to,
                                        1_000_000 + n,
                                    )],
                                    Some(&mint_keypair.pubkey()),
                                ),
                                blockhash,
                            );
                            sigs.push(tx.signatures[0]);
                            VersionedTransaction::from(tx)
                        })
                        .collect();
                    Entry {
                        num_hashes: 1,
                        hash: Hash::default(),
                        transactions: txs,
                    }
                })
                .collect();
            let shreds: Vec<Shred> = shredder
                .make_merkle_shreds_from_entries(
                    &keypair,
                    &entries,
                    b == 2,
                    Hash::default(),
                    next_index,
                    next_index,
                    &ReedSolomonCache::default(),
                    &mut ProcessShredsStats::default(),
                )
                .filter(Shred::is_data)
                .collect();
            let start = next_index;
            let end = shreds.iter().map(|s| s.index()).max().unwrap();
            next_index = end + 1;
            blockstore.insert_shreds(shreds, true).unwrap();
            batches.push((entries, start, end));
        }
        Fixture {
            _ledger: ledger,
            blockstore,
            bank_forks,
            batches,
            sigs,
            leader: SlotLeader {
                id: validator_pubkey,
                vote_address: voting_keypair.pubkey(),
            },
            mint_keypair,
        }
    }

    fn ingest(f: &Fixture, config: Config) -> (Ingest, Receiver<CoordMsg>, Receiver<CmpMsg>) {
        let (coord_tx, coord_rx) = crossbeam_channel::unbounded();
        let (cmp_tx, cmp_rx) = crossbeam_channel::unbounded();
        let ingest = Ingest::new(
            IngestDeps {
                bank_forks: f.bank_forks.clone(),
                blockstore: f.blockstore.clone(),
            },
            Arc::new(config),
            coord_tx,
            cmp_tx,
            Arc::new(Vec::new()),
        );
        (ingest, coord_rx, cmp_rx)
    }

    fn tap(sets: Vec<CompletedDataSetInfo>) -> TapBatch {
        TapBatch {
            sets,
            t: Instant::now(),
            t_unix_ns: 1,
        }
    }

    /// Transaction signatures handed to the coordinator, in order (checks contiguity); the
    /// run itself comes from the comparator's RunStart message.
    fn drain(
        coord_rx: &Receiver<CoordMsg>,
        cmp_rx: &Receiver<CmpMsg>,
        run: &mut Option<Arc<Run>>,
        next: &mut u32,
    ) -> (Vec<Signature>, Option<u32>) {
        let mut complete = None;
        let start = *next;
        while let Ok(msg) = coord_rx.try_recv() {
            match msg {
                CoordMsg::Txs { first, metas, .. } => {
                    assert_eq!(first, *next, "contiguous ordinals");
                    *next += metas.len() as u32;
                }
                CoordMsg::InputComplete { total, .. } => complete = Some(total),
                _ => {}
            }
        }
        for msg in cmp_rx.try_iter() {
            if let CmpMsg::RunStart { run: r, .. } = msg {
                *run = Some(r);
            }
        }
        let order = match run {
            Some(run) => (start..*next)
                .map(|k| run.tx(k).unwrap().signature)
                .collect(),
            None => Vec::new(),
        };
        (order, complete)
    }

    fn sets(f: &Fixture) -> Vec<CompletedDataSetInfo> {
        f.batches
            .iter()
            .map(|(_, start, end)| CompletedDataSetInfo {
                slot: 2,
                indices: *start..end + 1,
            })
            .collect()
    }

    #[test]
    fn test_blockstore_sets_out_of_order() {
        let f = fixture();
        let (mut ingest, coord_rx, cmp_rx) = ingest(&f, Config::default());
        let (mut run, mut next) = (None, 0);
        let mut s = sets(&f);
        s.reverse();
        ingest.on_tap(tap(vec![s[0].clone()]));
        ingest.on_tap(tap(vec![s[1].clone()]));
        assert!(
            drain(&coord_rx, &cmp_rx, &mut run, &mut next).0.is_empty(),
            "nothing before batch 0"
        );
        ingest.on_tap(tap(vec![s[2].clone()]));
        let (order, complete) = drain(&coord_rx, &cmp_rx, &mut run, &mut next);
        assert_eq!(order, f.sigs);
        assert_eq!(complete, Some(f.sigs.len() as u32));
    }

    #[test]
    fn test_dual_ring_first_then_blockstore() {
        let f = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ring");
        let mut writer = TestWriter::create(&path, 1 << 20);
        let mut config = Config::default();
        config.input_dual = true;
        config.ring_path = Some(path.clone());
        config.ingest_spin_us = 1_000_000;
        config.ring_blockstore_check = true;
        let (mut ingest, coord_rx, cmp_rx) = ingest(&f, config);
        assert!(!ingest.poll_ring(), "opens the ring, nothing to read yet");
        let meta = |start: u32, end: u32, offset: u32, count: u32, flags: u16| ring::RecordMeta {
            slot: 2,
            parent_slot: 1,
            batch_start: start,
            batch_end: end,
            entry_offset: offset,
            entry_count: count,
            flags,
            t_publish_ns: 42,
        };
        let (b0, s0, e0) = &f.batches[0];
        // Batch 0 streamed: 3 entries, then 1, then an empty FINAL marker.
        writer.publish(&meta(*s0, e0 - 1, 0, 3, 0), &wincode::serialize(&b0[..3].to_vec()).unwrap());
        writer.publish(&meta(*s0, e0 - 1, 3, 1, 0), &wincode::serialize(&b0[3..].to_vec()).unwrap());
        writer.publish(&meta(*s0, *e0, 4, 0, ring::FLAG_FINAL), &[]);
        let (b1, s1, e1) = &f.batches[1];
        writer.publish(&meta(*s1, *e1, 0, 4, ring::FLAG_FINAL), &wincode::serialize(b1).unwrap());
        let (b2, s2, e2) = &f.batches[2];
        writer.publish(
            &meta(*s2, *e2, 0, 4, ring::FLAG_FINAL | ring::FLAG_LAST_IN_SLOT),
            &wincode::serialize(b2).unwrap(),
        );
        assert!(ingest.poll_ring());
        let (mut run, mut next) = (None, 0);
        let (order, complete) = drain(&coord_rx, &cmp_rx, &mut run, &mut next);
        assert_eq!(order, f.sigs);
        assert_eq!(complete, Some(f.sigs.len() as u32));
        // Late blockstore copies are cross-checked and change nothing.
        ingest.on_tap(tap(sets(&f)));
        assert!(drain(&coord_rx, &cmp_rx, &mut run, &mut next).0.is_empty());
        assert!(ingest.slots[&2].gate.stats.cross_checked_entries >= 12);
        assert!(
            !cmp_rx
                .try_iter()
                .any(|m| matches!(m, CmpMsg::SlotSkipped { .. })),
            "no source mismatch"
        );
        assert_eq!(ingest.stats.ring_records, 5);
    }

    #[test]
    fn test_dual_mismatch_skips_slot() {
        let f = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ring");
        let mut writer = TestWriter::create(&path, 1 << 20);
        let mut config = Config::default();
        config.input_dual = true;
        config.ring_path = Some(path.clone());
        config.ingest_spin_us = 1_000_000;
        config.ring_blockstore_check = true;
        let (mut ingest, _coord_rx, cmp_rx) = ingest(&f, config);
        ingest.poll_ring();
        // The ring claims batch 0 holds batch 1's entries.
        let (_, s0, e0) = &f.batches[0];
        let (b1, _, _) = &f.batches[1];
        writer.publish(
            &ring::RecordMeta {
                slot: 2,
                parent_slot: 1,
                batch_start: *s0,
                batch_end: *e0,
                entry_offset: 0,
                entry_count: 4,
                flags: ring::FLAG_FINAL,
                t_publish_ns: 1,
            },
            &wincode::serialize(b1).unwrap(),
        );
        ingest.poll_ring();
        ingest.on_tap(tap(vec![sets(&f)[0].clone()]));
        assert!(cmp_rx.try_iter().any(|m| matches!(
            m,
            CmpMsg::SlotSkipped {
                reason: "source_mismatch",
                ..
            }
        )));
    }

    #[test]
    fn test_chain_on_unfrozen_parent() {
        use crate::sched::{RunSummary, SchedRun};
        let f = fixture();
        // Agave's bank 2: created over the frozen bank 1, not frozen.
        let bank1 = f.bank_forks.read().unwrap().get(1).unwrap();
        let bank2 =
            Bank::new_from_parent_with_bank_forks(&f.bank_forks, bank1.clone(), f.leader, 2);
        // Slot 3 (parent 2): one batch, last in slot, spending a fresh transfer.
        let entries = vec![Entry {
            num_hashes: 1,
            hash: Hash::default(),
            transactions: vec![VersionedTransaction::from(Transaction::new(
                &[&f.mint_keypair],
                Message::new(
                    &[system_instruction::transfer(
                        &f.mint_keypair.pubkey(),
                        &Pubkey::new_unique(),
                        3_000_000,
                    )],
                    Some(&f.mint_keypair.pubkey()),
                ),
                bank1.last_blockhash(),
            ))],
        }];
        let shreds: Vec<Shred> = Shredder::new(3, 2, 0, 0)
            .unwrap()
            .make_merkle_shreds_from_entries(
                &Keypair::new(),
                &entries,
                true,
                Hash::default(),
                0,
                0,
                &ReedSolomonCache::default(),
                &mut ProcessShredsStats::default(),
            )
            .filter(Shred::is_data)
            .collect();
        let end = shreds.iter().map(|s| s.index()).max().unwrap();
        f.blockstore.insert_shreds(shreds, true).unwrap();

        let mut config = Config::default();
        config.chain = true;
        let (mut ingest, coord_rx, cmp_rx) = ingest(&f, config);
        // FL runs slot 2 (its parent is frozen).
        ingest.on_tap(tap(sets(&f)));
        let (mut run_p, mut next) = (None, 0);
        let (_, complete) = drain(&coord_rx, &cmp_rx, &mut run_p, &mut next);
        assert_eq!(complete, Some(f.sigs.len() as u32));
        let run_p = run_p.unwrap();
        assert!(ingest.slots[&2].done_run.is_some());
        // Slot 3's data arrives while FL's run of 2 is still executing: it waits.
        ingest.on_tap(tap(vec![CompletedDataSetInfo {
            slot: 3,
            indices: 0..end + 1,
        }]));
        assert!(coord_rx.try_iter().next().is_none(), "no run before 2 completes");
        // FL's run of 2 completes; the child chains on it.
        run_p.on_complete(&RunSummary::default());
        let slot = ingest.complete_rx.try_recv().unwrap();
        ingest.on_run_complete(slot);
        assert!(ingest.stats.slots_skipped.is_empty(), "{:?}", ingest.stats.slots_skipped);
        let msgs: Vec<CoordMsg> = coord_rx.try_iter().collect();
        let run_c = cmp_rx
            .try_iter()
            .find_map(|m| match m {
                CmpMsg::RunStart { run, .. } => Some(run),
                _ => None,
            })
            .unwrap();
        let chain = run_c.chain.as_ref().unwrap();
        assert_eq!(chain.parent_run.slot, 2);
        assert_eq!(run_c.ordinal_base, 1);
        assert!(matches!(msgs[0], CoordMsg::NewRun { .. }));
        match &msgs[1] {
            CoordMsg::Txs { first: 0, metas, .. } => {
                assert!(metas.len() == 1 && metas[0].external);
                assert_eq!(metas[0].locks.len(), 4);
            }
            _ => panic!("pseudo-transaction first"),
        }
        assert!(matches!(msgs[2], CoordMsg::Txs { first: 1, .. }));
        assert!(matches!(msgs[3], CoordMsg::InputComplete { total: 2, .. }));
        assert_eq!(run_c.tx(1).unwrap().signature, entries[0].transactions[0].signatures[0]);
        assert_eq!(ingest.stats.runs_chained, 1);
        // Agave freezes 2: the pseudo-transaction completes with the frozen values.
        bank2.freeze();
        ingest.on_event(AgaveEvent::Frozen {
            slot: 2,
            bank_id: bank2.bank_id(),
            parent_slot: 1,
            t: Instant::now(),
        });
        let done = coord_rx
            .try_iter()
            .find_map(|m| match m {
                CoordMsg::ExternalDone { k: 0, out, .. } => Some(out),
                _ => None,
            })
            .unwrap();
        let collector = bank2.fast_lane_collector_id().unwrap();
        let slot_hashes_id = solana_sdk_ids::sysvar::slot_hashes::id();
        assert!(done.writes.iter().any(|(k, _)| *k == collector));
        let (_, slot_hashes) = done
            .writes
            .iter()
            .find(|(k, _)| *k == slot_hashes_id)
            .unwrap();
        let expected = solana_account::from_account::<solana_slot_hashes::SlotHashes, _>(
            slot_hashes,
        )
        .unwrap();
        assert_eq!(expected.get(&2), Some(&bank2.hash()));
        assert!(chain.resolved.load(Ordering::SeqCst));
        assert!(ingest.slots[&2].done_run.is_none());
        assert!(ingest.slots[&3].chain_run.is_none());
        assert_eq!(ingest.stats.chains_resolved, 1);
    }

    /// Regression (2026-09-28 OOMs): agave's rooted notifications never reach the slot-status
    /// tee, so FL used to hold every frozen bank forever. FL now follows bank_forks' root and
    /// keeps only live banks at or above it.
    #[test]
    fn test_banks_released_without_rooted_events() {
        let f = fixture();
        let (mut ingest, _coord_rx, _cmp_rx) = ingest(&f, Config::default());
        let mut weaks = Vec::new();
        let mut parent = f.bank_forks.read().unwrap().get(1).unwrap();
        for slot in 2..=300u64 {
            let bank = Bank::new_from_parent_with_bank_forks(
                &f.bank_forks,
                parent.clone(),
                SlotLeader::default(),
                slot,
            );
            bank.freeze();
            weaks.push(Arc::downgrade(&bank));
            // What the block-metadata tee sends; no Rooted event ever arrives.
            ingest.on_event(AgaveEvent::Frozen {
                slot,
                bank_id: bank.bank_id(),
                parent_slot: slot - 1,
                t: Instant::now(),
            });
            if slot > 40 {
                f.bank_forks.write().unwrap().set_root(slot - 32, None, None);
            }
            ingest.last_bank_prune = Instant::now() - Duration::from_secs(1);
            ingest.housekeeping();
            parent = bank;
        }
        drop(parent);
        let alive = weaks.iter().filter(|w| w.upgrade().is_some()).count();
        let in_forks = f.bank_forks.read().unwrap().banks().len();
        assert_eq!(ingest.root, 268);
        assert!(ingest.frozen.len() <= 33, "held {}", ingest.frozen.len());
        assert!(alive <= in_forks, "alive {alive} > {in_forks} in bank_forks");
        assert_eq!(crate::mem::BANKS_HELD.get() as usize, ingest.frozen.len());

        // Releasing (the fast lane turned off) drops every bank FL holds.
        ingest.release("disabled");
        assert_eq!(ingest.frozen.len(), 0);
        assert!(ingest.slots.is_empty());
    }
}
