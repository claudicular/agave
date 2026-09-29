#![cfg(feature = "agave-unstable-api")]
//! The speculative fast lane: an in-process, non-mutating, optimistic-parallel executor for
//! replayed slots, run in shadow against agave's own replay.
//!
//! Phase 1 (this crate): for each slot N whose parent P agave has frozen, FL reads N's
//! entries from the blockstore as soon as agave's window service completes each data set,
//! executes them with agave's own SVM over P and a private multi-version overlay (lock-aware
//! Block-STM, [`sched`]), and compares every transaction's account frame with agave's
//! grouped `transaction_accounts` notification ([`compare`]). It never writes agave state,
//! never takes agave write locks, and is off unless `AGAVE_FAST_LANE_CONFIG` enables it.
//! Design: `docs/research/shred-to-geyser-2026-09-27/fast_lane/DESIGN.md` (arb_bot repo).
//!
//! Agave integration points (all no-ops unless enabled at boot):
//! - `window_service::run_insert` calls [`tap::on_completed_data_sets`];
//! - `Validator::new` wraps the geyser notifiers with [`FastLane::tee_accounts_update`],
//!   [`FastLane::tee_block_metadata`] and [`FastLane::tee_slot_status`], then calls
//!   [`FastLane::start`] once `bank_forks` and the blockstore exist;
//! - `cluster_slot_state_verifier` and the vote listener call [`cluster_check`], which poisons
//!   the fast lane when the cluster's bank hash for a slot differs from ours.

pub mod cluster_check;
pub mod commit;
pub mod compare;
pub mod config;
pub mod control;
pub mod export;
pub mod forks;
pub mod full_cmp;
pub mod gate;
pub mod ingest;
pub mod mem;
pub mod mv;
pub mod out_ring;
pub mod output;
pub mod program_cache;
pub mod ring;
pub mod run;
pub mod safety;
pub mod sched;
pub mod tap;
pub mod tees;

use {
    crate::{
        compare::{CmpMsg, CmpSink, Comparator},
        config::Config,
        control::{TapStats, Tunables},
        ingest::{Ingest, IngestDeps},
        safety::Placement,
        sched::{CoordMsg, Coordinator, worker_loop},
        tap::TapBatch,
        tees::{AgaveEvent, AgaveFrame},
    },
    crossbeam_channel::{Receiver, Sender, bounded, unbounded},
    log::{error, info, warn},
    solana_accounts_db::accounts_update_notifier_interface::AccountsUpdateNotifier,
    solana_geyser_plugin_manager::block_metadata_notifier_interface::BlockMetadataNotifierArc,
    solana_ledger::blockstore::Blockstore,
    solana_pubkey::Pubkey,
    solana_rpc::slot_status_notifier::SlotStatusNotifier,
    solana_runtime::{
        bank_forks::BankForks,
        fast_lane_commit::{AgaveProcessed, FastLaneHooks},
        prioritization_fee_cache::PrioritizationFeeCache,
        transaction_execution::TransactionStatusSender,
        vote_sender_types::ReplayVoteSender,
    },
    std::{
        path::PathBuf,
        sync::{
            Arc, Mutex, OnceLock, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread::JoinHandle,
        time::{Duration, Instant},
    },
};

/// Channels shared between agave-side taps/tees and FL threads.
pub struct Shared {
    pub(crate) tap_tx: Sender<TapBatch>,
    pub(crate) event_tx: Sender<AgaveEvent>,
    pub(crate) frame_tx: Sender<AgaveFrame>,
    pub(crate) tap_stats: TapStats,
    pub(crate) readonly_owners: OnceLock<Vec<Pubkey>>,
    pub(crate) comparator_enabled: bool,
    receivers: Mutex<Option<Receivers>>,
}

struct Receivers {
    tap_rx: Receiver<TapBatch>,
    event_rx: Receiver<AgaveEvent>,
    frame_rx: Receiver<AgaveFrame>,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

pub(crate) fn shared() -> Option<&'static Arc<Shared>> {
    SHARED.get()
}

/// Built at validator start (before the notifiers are handed out).
pub struct FastLane {
    config: Option<Arc<Config>>,
    shared: Option<Arc<Shared>>,
}

/// What `start` needs from the validator.
pub struct FastLaneDeps {
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub blockstore: Arc<Blockstore>,
    pub exit: Arc<AtomicBool>,
    pub ledger_path: PathBuf,
    /// The services replay's commit feeds (the unified scheduler's handler context): FL
    /// executes with the same recording configuration.
    pub replay: ReplayServices,
}

/// What agave's replay hands each committed transaction to (the handler context of the
/// unified scheduler, `DefaultTaskHandler`).
#[derive(Clone, Default)]
pub struct ReplayServices {
    pub transaction_status_sender: Option<TransactionStatusSender>,
    pub replay_vote_sender: Option<ReplayVoteSender>,
    pub prioritization_fee_cache: Option<Arc<PrioritizationFeeCache>>,
    pub log_messages_bytes_limit: Option<usize>,
}

/// The fast lane's side of agave's replay hooks (`solana_runtime::fast_lane_commit`). Runs on
/// agave's replay handler threads: wait-free (`try_send`), never panics.
pub struct FlHooks {
    pub full_tx: Sender<Box<AgaveProcessed>>,
    pub full_drops: Arc<AtomicU64>,
    /// The coordinator's channel (unbounded: sending never blocks agave).
    pub coord_tx: Sender<CoordMsg>,
}

impl FastLaneHooks for FlHooks {
    fn on_agave_done(&self, _slot: solana_clock::Slot, bank_id: solana_clock::BankId, index: usize) {
        let _ = self.coord_tx.send(CoordMsg::Sink(Box::new(commit::CommitEvent::AgaveDone {
            bank_id,
            index,
        })));
    }

    fn on_bank_inserted(&self, bank: &Arc<solana_runtime::bank::Bank>) {
        if control::committing() {
            let _ = self
                .coord_tx
                .send(CoordMsg::Sink(Box::new(commit::CommitEvent::BankInserted(
                    Arc::clone(bank),
                ))));
        }
    }

    fn poison(&self, reason: &'static str) {
        control::poison(reason);
    }

    fn on_agave_processed(&self, processed: AgaveProcessed) {
        if !control::is_active() || !control::keep_processed() {
            return;
        }
        let bytes = full_cmp::agave_bytes(&processed);
        mem::FULL_BYTES.add(bytes);
        if self.full_tx.try_send(Box::new(processed)).is_err() {
            mem::FULL_BYTES.sub(bytes);
            self.full_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub struct FastLaneHandle {
    exit: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl FastLaneHandle {
    /// Stop FL threads; waits at most ~2 s in total and never blocks shutdown beyond that.
    pub fn join(self) {
        self.exit.store(true, Ordering::SeqCst);
        control::set_active(false);
        solana_runtime::fast_lane_commit::uninstall_hooks();
        let deadline = Instant::now() + Duration::from_secs(2);
        for thread in self.threads {
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

impl FastLane {
    pub fn disabled() -> Self {
        Self {
            config: None,
            shared: None,
        }
    }

    /// Read `AGAVE_FAST_LANE_CONFIG`. Any error disables the fast lane (logged).
    pub fn prepare_from_env() -> Self {
        match Config::from_env() {
            Ok(Some(config)) if config.enabled => Self::prepare(config),
            Ok(Some(_)) => {
                info!("fast lane: configured but enabled=false");
                Self::disabled()
            }
            Ok(None) => Self::disabled(),
            Err(err) => {
                error!("{err}; fast lane disabled");
                Self::disabled()
            }
        }
    }

    pub fn prepare(config: Config) -> Self {
        let (tap_tx, tap_rx) = bounded(4096);
        let (event_tx, event_rx) = bounded(4096);
        let (frame_tx, frame_rx) = bounded(65_536);
        let shared = Arc::new(Shared {
            tap_tx,
            event_tx,
            frame_tx,
            tap_stats: TapStats::default(),
            readonly_owners: OnceLock::new(),
            comparator_enabled: config.comparator,
            receivers: Mutex::new(Some(Receivers {
                tap_rx,
                event_rx,
                frame_rx,
            })),
        });
        if SHARED.set(Arc::clone(&shared)).is_err() {
            warn!("fast lane: already prepared in this process; using the first instance");
            return Self::disabled();
        }
        info!("fast lane: prepared ({config:?})");
        Self {
            config: Some(Arc::new(config)),
            shared: Some(shared),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.is_some()
    }

    pub fn tee_accounts_update(
        &self,
        inner: Option<AccountsUpdateNotifier>,
    ) -> Option<AccountsUpdateNotifier> {
        match &self.shared {
            Some(shared) => tees::tee_accounts_update(inner, shared),
            None => inner,
        }
    }

    pub fn tee_block_metadata(
        &self,
        inner: Option<BlockMetadataNotifierArc>,
    ) -> Option<BlockMetadataNotifierArc> {
        match &self.shared {
            Some(shared) => tees::tee_block_metadata(inner, shared),
            None => inner,
        }
    }

    pub fn tee_slot_status(&self, inner: Option<SlotStatusNotifier>) -> Option<SlotStatusNotifier> {
        match &self.shared {
            Some(shared) => tees::tee_slot_status(inner, shared),
            None => inner,
        }
    }

    /// Spawn FL's threads and activate it. Must run after agave installed its panic hook.
    pub fn start(self, deps: FastLaneDeps) -> Option<FastLaneHandle> {
        let (Some(config), Some(shared)) = (self.config, self.shared) else {
            return None;
        };
        match start_threads(config, shared, deps) {
            Ok(handle) => Some(handle),
            Err(err) => {
                error!("fast lane: failed to start: {err}");
                control::set_active(false);
                None
            }
        }
    }
}

fn placement(core: Option<usize>, nice: i32, shared: &[usize]) -> Placement {
    match core {
        Some(core) => Placement::Pinned(vec![core]),
        None => Placement::Niced(nice, shared.to_vec()),
    }
}

fn start_threads(
    config: Arc<Config>,
    shared: Arc<Shared>,
    deps: FastLaneDeps,
) -> Result<FastLaneHandle, String> {
    let receivers = shared
        .receivers
        .lock()
        .map_err(|_| "receivers lock poisoned".to_string())?
        .take()
        .ok_or("fast lane already started")?;
    safety::install_panic_hook();
    let exit = Arc::new(AtomicBool::new(false));
    let tunables = Arc::new(
        Tunables::new(
            config.speculation,
            config.eager_reexec,
            config.theta,
            config.max_incarnations,
        )
        .with_rebase(config.rebase),
    );
    control::set_vm_opts(config.vm_opts);
    control::set_agave_execution(control::AgaveExecution {
        record: deps.replay.transaction_status_sender.is_some(),
        log_messages_bytes_limit: deps.replay.log_messages_bytes_limit,
    });
    let readonly_owners = Arc::new(shared.readonly_owners.get().cloned().unwrap_or_default());
    info!(
        "fast lane: starting {} workers (cores {:?}), readonly owners {:?}",
        config.workers, config.worker_cores, readonly_owners
    );
    let export_dir = config
        .export_dir
        .clone()
        .unwrap_or_else(|| deps.ledger_path.join("fast_lane"));
    let control_file = config
        .control_file
        .clone()
        .unwrap_or_else(|| deps.ledger_path.join("fast_lane.ctl"));
    let spin = Duration::from_micros(config.spin_us);
    let worker_spin = Duration::from_micros(config.worker_spin_us);

    let (coord_tx, coord_rx) = unbounded::<CoordMsg>();
    let (task_tx, task_rx) = unbounded();
    let (cmp_tx, cmp_rx) = bounded::<CmpMsg>(65_536);
    let (full_tx, full_rx) = bounded::<Box<AgaveProcessed>>(65_536);
    let (job_tx, job_rx) = unbounded::<commit::CommitJob>();
    let commit_metrics = Arc::new(commit::CommitMetrics::default());
    control::set_verify_sample_ppm(config.verify_sample_ppm);
    control::set_follow_wait_ms(config.follow_wait_ms);
    solana_runtime::fast_lane_commit::set_follow_spin(Duration::from_micros(config.follow_spin_us));
    commit::log_start(&config);
    let full_drops = Arc::new(AtomicU64::new(0));
    let sink_drops = Arc::new(AtomicU64::new(0));
    // Phase-3 output ring (created before any thread so its failure only disables it).
    let out_stats = Arc::new(output::OutStats::default());
    let out = if config.out_ring {
        match out_ring::OutRing::create(&config.out_ring_path, config.out_ring_mb << 20) {
            Ok(ring) => {
                let owners = output::OutPublisher::owner_filter(config.out_token, &config.out_owners);
                info!(
                    "fast lane: output ring {} ({} MiB, max record {} B), owners {:?}",
                    config.out_ring_path.display(),
                    config.out_ring_mb,
                    ring.max_record(),
                    owners
                );
                Some(output::OutPublisher::new(ring, owners, out_stats.clone()))
            }
            Err(err) => {
                log::warn!(
                    "fast lane: output ring {} disabled: {err}",
                    config.out_ring_path.display()
                );
                None
            }
        }
    } else {
        None
    };
    let out_enabled = out.is_some();
    let mut threads = Vec::new();
    let spawn_err = |e: std::io::Error| e.to_string();

    // Executors.
    for i in 0..config.workers {
        let place = if config.worker_cores.is_empty() {
            Placement::Niced(config.nice, config.shared_cores.clone())
        } else {
            Placement::Pinned(vec![config.worker_cores[i % config.worker_cores.len()]])
        };
        let task_rx = task_rx.clone();
        let coord_tx = coord_tx.clone();
        let exit = exit.clone();
        threads.push(
            safety::spawn(&format!("solFlExec{i:02}"), place, move || {
                worker_loop(task_rx, coord_tx, exit, worker_spin)
            })
            .map_err(spawn_err)?,
        );
    }
    drop(task_rx);

    // Commit threads (commit mode): commit FINAL transactions into agave's banks.
    for i in 0..config.commit_threads {
        let place = if config.commit_cores.is_empty() {
            Placement::Niced(config.nice, config.shared_cores.clone())
        } else {
            Placement::Pinned(vec![config.commit_cores[i % config.commit_cores.len()]])
        };
        let job_rx = job_rx.clone();
        let coord_tx = coord_tx.clone();
        let exit = exit.clone();
        let services = deps.replay.clone();
        let metrics = commit_metrics.clone();
        let spin = Duration::from_micros(config.commit_spin_us);
        threads.push(
            safety::spawn(&format!("solFlCommit{i:02}"), place, move || {
                commit::commit_worker_loop(job_rx, coord_tx, services, exit, spin, metrics)
            })
            .map_err(spawn_err)?,
        );
    }
    drop(job_rx);

    // Coordinator.
    {
        let exit = exit.clone();
        let tunables = tunables.clone();
        let sink = CmpSink {
            tx: cmp_tx.clone(),
            drops: sink_drops.clone(),
            out,
            committer: Some(commit::Committer::new(
                job_tx,
                Some(deps.bank_forks.clone()),
                commit_metrics.clone(),
            )),
        };
        let workers = config.workers;
        let hint_alpha = config.hint_alpha;
        threads.push(
            safety::spawn(
                "solFlSched",
                placement(config.sched_core, config.nice, &config.shared_cores),
                move || {
                    let mut coordinator =
                        Coordinator::new(workers, task_tx, tunables, hint_alpha, sink);
                    coordinator.run_loop(coord_rx, exit, spin);
                },
            )
            .map_err(spawn_err)?,
        );
    }

    // Ingest.
    {
        let exit_ingest = exit.clone();
        let config_c = config.clone();
        let coord_tx = coord_tx.clone();
        let cmp_tx = cmp_tx.clone();
        let deps_ingest = IngestDeps {
            bank_forks: deps.bank_forks.clone(),
            blockstore: deps.blockstore.clone(),
        };
        let readonly_owners = readonly_owners.clone();
        let Receivers {
            tap_rx,
            event_rx,
            frame_rx,
        } = receivers;
        threads.push(
            safety::spawn(
                "solFlIngest",
                placement(config.ingest_core, config.nice, &config.shared_cores),
                move || {
                    let mut ingest =
                        Ingest::new(deps_ingest, config_c, coord_tx, cmp_tx, readonly_owners);
                    ingest.run_loop(tap_rx, event_rx, exit_ingest);
                },
            )
            .map_err(spawn_err)?,
        );

        // Comparator.
        let exit = exit.clone();
        let config_c = config.clone();
        let bank_forks = deps.bank_forks.clone();
        let shared_c = shared.clone();
        let full_drops_c = full_drops.clone();
        threads.push(
            safety::spawn("solFlCmp", Placement::Niced(config.aux_nice, config.shared_cores.clone()), move || {
                let mut comparator =
                    Comparator::new(config_c, bank_forks, export_dir, sink_drops);
                comparator.tap_stats = Some(shared_c);
                comparator.full_drops = full_drops_c;
                comparator.commit_metrics = Some(commit_metrics);
                if out_enabled {
                    comparator.out_stats = Some(out_stats);
                }
                comparator.run_loop(cmp_rx, frame_rx, full_rx, exit);
            })
            .map_err(spawn_err)?,
        );
    }
    let hooks_coord_tx = coord_tx.clone();
    drop(coord_tx);

    // Control.
    {
        let exit_fl = exit.clone();
        let exit_validator = deps.exit.clone();
        let allow_panic = config.allow_panic_command;
        let tunables = tunables.clone();
        threads.push(
            safety::spawn(
                "solFlCtl",
                Placement::Niced(config.aux_nice, config.shared_cores.clone()),
                move || {
                    let mut last = None;
                    loop {
                        if exit_fl.load(Ordering::Relaxed)
                            || exit_validator.load(Ordering::Relaxed)
                        {
                            exit_fl.store(true, Ordering::SeqCst);
                            control::set_active(false);
                            return;
                        }
                        if control::poll_control_file(
                            &control_file,
                            &mut last,
                            &tunables,
                            allow_panic,
                        ) {
                            panic!("fast lane: panic drill requested by control file");
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                },
            )
            .map_err(spawn_err)?,
        );
    }

    solana_runtime::fast_lane_commit::install_hooks(Arc::new(FlHooks {
        full_tx,
        full_drops,
        coord_tx: hooks_coord_tx,
    }));
    control::set_active(true);
    control::set_commit_mode(config.commit);
    info!(
        "fast lane: active (commit={}, agave records statuses: {})",
        control::commit_mode_name(config.commit),
        control::agave_execution().record
    );
    Ok(FastLaneHandle { exit, threads })
}
