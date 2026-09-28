//! Ingest: slot lifecycle, the data-set ordering gate, blockstore reads, sanitization and
//! run creation. Runs on `solFlIngest`.
//!
//! For each slot N FL keeps the contiguous prefix of completed data sets (agave's replay is
//! equally blocked on gaps). Once N's parent P is frozen (block-metadata tee), FL computes
//! N's environment from P, creates a run, sanitizes each released transaction exactly as
//! replay does (child-slot ALT resolution, lock validation, static checks) and hands them to
//! the coordinator in ledger order. Transaction ordinals count every transaction of the slot
//! from shred 0, which equals agave's `transaction_indexes`.

use {
    crate::{
        compare::CmpMsg,
        config::Config,
        forks::FrozenBanks,
        mv::TxIdx,
        program_cache::ProgramCaches,
        run::Run,
        sched::{CoordMsg, RunId},
        tap::TapBatch,
        tees::AgaveEvent,
    },
    crossbeam_channel::{Receiver, Sender, select},
    log::info,
    solana_clock::Slot,
    solana_entry::entry::Entry,
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
    next_shred: u32,
    pending: BTreeMap<u32, (u32, Instant, u64)>,
    released: Vec<ReleasedSet>,
    run: Option<(RunId, Arc<Run>)>,
    /// Kept after the run's input completes (the Arc is released then).
    run_id: Option<RunId>,
    next_ordinal: TxIdx,
    status: SlotStatus,
    parent_frozen_at_first_set: bool,
    input_complete: bool,
}

#[derive(Default, Debug, Clone)]
pub struct IngestStats {
    pub data_sets: u64,
    pub duplicate_sets: u64,
    pub blockstore_read_us: u64,
    pub sanitize_us: u64,
    pub txs: u64,
    pub runs_started: u64,
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
    pub stats: IngestStats,
}

impl Ingest {
    pub fn new(
        deps: IngestDeps,
        config: Arc<Config>,
        coord_tx: Sender<CoordMsg>,
        cmp_tx: Sender<CmpMsg>,
        readonly_owners: Arc<Vec<Pubkey>>,
    ) -> Self {
        let fork_graph = Arc::new(RwLock::new(crate::forks::FlForkGraph::default()));
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
            stats: IngestStats::default(),
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
        loop {
            if exit.load(Ordering::Relaxed) {
                return;
            }
            select! {
                recv(tap_rx) -> msg => match msg {
                    Ok(batch) => self.on_tap(batch),
                    Err(_) => return,
                },
                recv(event_rx) -> msg => match msg {
                    Ok(event) => self.on_event(event),
                    Err(_) => return,
                },
                default(Duration::from_millis(20)) => {},
            }
            if !crate::control::is_active() {
                self.abort_all("disabled");
            }
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
            state.pending.clear();
            if let Some((run_id, _)) = state.run.take() {
                let _ = self.coord_tx.send(CoordMsg::AbortRun { run_id, reason });
                self.programs.prune_slot(slot);
            }
        }
        *self.stats.slots_skipped.entry(reason).or_default() += 1;
        let _ = self.cmp_tx.try_send(CmpMsg::SlotSkipped { slot, reason });
    }

    fn abort_all(&mut self, reason: &'static str) {
        let slots: Vec<Slot> = self
            .slots
            .iter()
            .filter(|(_, s)| matches!(s.status, SlotStatus::Collecting | SlotStatus::Running))
            .map(|(slot, _)| *slot)
            .collect();
        for slot in slots {
            self.skip(slot, reason);
        }
    }

    fn housekeeping(&mut self) {
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
                let children: Vec<Slot> = self
                    .slots
                    .iter()
                    .filter(|(_, s)| s.parent == Some(slot) && s.run.is_none())
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

    fn on_tap(&mut self, batch: TapBatch) {
        let mut touched = Vec::new();
        for set in &batch.sets {
            let slot = set.slot;
            if slot <= self.root || self.dead.contains_key(&slot) {
                continue;
            }
            self.stats.data_sets += 1;
            if !self.slots.contains_key(&slot) {
                let parent = self
                    .deps
                    .blockstore
                    .meta(slot)
                    .ok()
                    .flatten()
                    .and_then(|meta| meta.parent_slot);
                let parent_frozen = parent.is_some_and(|p| self.frozen.get(p).is_some());
                self.slots.insert(
                    slot,
                    SlotIngest {
                        parent,
                        first_seen: batch.t,
                        next_shred: 0,
                        pending: BTreeMap::new(),
                        released: Vec::new(),
                        run: None,
                        run_id: None,
                        next_ordinal: 0,
                        status: SlotStatus::Collecting,
                        parent_frozen_at_first_set: parent_frozen,
                        input_complete: false,
                    },
                );
            }
            let Some(state) = self.slots.get_mut(&slot) else {
                continue;
            };
            if matches!(state.status, SlotStatus::Skipped(_) | SlotStatus::Complete) {
                continue;
            }
            if set.indices.start < state.next_shred || state.pending.contains_key(&set.indices.start)
            {
                // Duplicate delivery (or a restarted slot: agave will mark it dead/replaced).
                self.stats.duplicate_sets += 1;
                continue;
            }
            state
                .pending
                .insert(set.indices.start, (set.indices.end, batch.t, batch.t_unix_ns));
            if !touched.contains(&slot) {
                touched.push(slot);
            }
        }
        for slot in touched {
            if let Err(reason) = self.release(slot) {
                self.skip(slot, reason);
                continue;
            }
            self.try_start_run(slot);
        }
    }

    /// Move the contiguous prefix of pending data sets into `released`.
    fn release(&mut self, slot: Slot) -> Result<(), &'static str> {
        let blockstore = Arc::clone(&self.deps.blockstore);
        let Some(state) = self.slots.get_mut(&slot) else {
            return Ok(());
        };
        let mut released_any = false;
        while let Some((end, t_tap, t_tap_unix_ns)) = state.pending.remove(&state.next_shred) {
            let start = state.next_shred;
            let t0 = Instant::now();
            let entries = blockstore
                .get_entries_in_data_block(slot, start..end, None)
                .map_err(|_| "blockstore_read")?;
            self.stats.blockstore_read_us += t0.elapsed().as_micros() as u64;
            state.released.push(ReleasedSet {
                entries,
                t_tap,
                t_tap_unix_ns,
            });
            state.next_shred = end;
            released_any = true;
        }
        if released_any && !state.input_complete {
            if let Ok(Some(meta)) = blockstore.meta(slot) {
                if state.parent.is_none() {
                    state.parent = meta.parent_slot;
                }
                if let Some(last_index) = meta.last_index {
                    if u64::from(state.next_shred) > last_index {
                        state.input_complete = true;
                    }
                }
            }
        }
        Ok(())
    }

    fn active_runs(&self) -> usize {
        self.slots
            .values()
            .filter(|s| s.status == SlotStatus::Running)
            .count()
    }

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
        let Some(parent) = self.frozen.get(parent_slot).cloned() else {
            return;
        };
        if self.active_runs() >= self.config.max_runs {
            return;
        }
        if self.overlay_bytes() > self.config.mem_cap_mb.saturating_mul(1 << 20) {
            self.skip(slot, "mem_cap");
            return;
        }
        match self.create_run(slot, &parent) {
            Ok(()) => self.feed(slot),
            Err(reason) => self.skip(slot, reason),
        }
    }

    fn create_run(&mut self, slot: Slot, parent: &Arc<Bank>) -> Result<(), &'static str> {
        let t0 = Instant::now();
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        let run = Arc::new(
            Run::new(
                run_id,
                slot,
                parent,
                &mut self.programs,
                Arc::clone(&self.readonly_owners),
            )
            .map_err(unsupported_reason)?,
        );
        let Some(state) = self.slots.get_mut(&slot) else {
            return Err("gone");
        };
        state.run = Some((run_id, Arc::clone(&run)));
        state.run_id = Some(run_id);
        state.status = SlotStatus::Running;
        let parent_wait_us = state.first_seen.elapsed().as_micros() as u64;
        let parent_frozen_at_first_set = state.parent_frozen_at_first_set;
        self.stats.runs_started += 1;
        self.coord_tx
            .send(CoordMsg::NewRun {
                run_id,
                run: run.clone(),
            })
            .map_err(|_| "coordinator_gone")?;
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
        for set in released {
            let t0 = Instant::now();
            let first = ordinal;
            let (metas, set_failure) = run.push_entries(set.entries, set.t_tap, set.t_tap_unix_ns);
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
            if set_failure.is_some() {
                failure = set_failure;
                break;
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
        if state.input_complete && state.released.is_empty() && state.pending.is_empty() {
            state.status = SlotStatus::Complete;
            // The coordinator and comparator hold the run from here on.
            state.run = None;
            let _ = self.coord_tx.send(CoordMsg::InputComplete {
                run_id,
                total: ordinal,
            });
        }
    }
}

fn unsupported_reason(err: solana_runtime::bank::fast_lane::FastLaneUnsupported) -> &'static str {
    use solana_runtime::bank::fast_lane::FastLaneUnsupported as U;
    match err {
        U::ParentNotFrozen => "parent_not_frozen",
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
