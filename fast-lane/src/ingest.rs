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
            ring: None,
            ring_retry_at: Instant::now(),
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
                    default(Duration::from_millis(20)) => {},
                }
                idle_since = Instant::now();
            } else {
                // Busy-poll (dedicated core): the ring first (earliest input), then taps.
                let mut worked = self.poll_ring();
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
                if worked {
                    idle_since = Instant::now();
                } else {
                    std::hint::spin_loop();
                    if last_housekeeping.elapsed() < Duration::from_millis(20) {
                        continue;
                    }
                }
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
            genesis_utils::{GenesisConfigInfo, create_genesis_config},
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
    }

    fn fixture() -> Fixture {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1_000_000_000_000);
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
}
