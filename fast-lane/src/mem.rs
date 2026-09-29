//! Accounting of the memory the fast lane holds, per structure, and the hard cap.
//!
//! Gauges are process-wide atomics updated where memory is taken and released (account
//! data counted as `data.len() + ACCOUNT_OVERHEAD`). Ingest checks the total against
//! `mem_cap_mb` every housekeeping pass (~20 ms); over the cap the fast lane disables itself
//! and releases everything it holds (it stays off until `enable` in the control file).
//! The comparator reports the breakdown in `fast_lane_mem` every summary interval.

use {
    solana_account::{AccountSharedData, ReadableAccount},
    std::sync::atomic::{AtomicI64, AtomicU64, Ordering},
};

/// Approximate per-account overhead beyond its data (key, struct, Arc, map slot).
pub const ACCOUNT_OVERHEAD: i64 = 128;

pub fn account_bytes(account: &AccountSharedData) -> i64 {
    account.data().len() as i64 + ACCOUNT_OVERHEAD
}

pub fn frame_bytes<'a>(accounts: impl IntoIterator<Item = &'a AccountSharedData>) -> i64 {
    accounts.into_iter().map(account_bytes).sum()
}

/// A byte (or count) gauge.
pub struct Gauge(AtomicI64);

impl Gauge {
    pub const fn new() -> Self {
        Self(AtomicI64::new(0))
    }
    #[inline]
    pub fn add(&self, v: i64) {
        self.0.fetch_add(v, Ordering::Relaxed);
    }
    #[inline]
    pub fn sub(&self, v: i64) {
        self.0.fetch_sub(v, Ordering::Relaxed);
    }
    pub fn set(&self, v: i64) {
        self.0.store(v, Ordering::Relaxed);
    }
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed).max(0)
    }
}

impl Default for Gauge {
    fn default() -> Self {
        Self::new()
    }
}

/// Account versions and cached base reads of every live overlay (live runs).
pub static OVERLAY_BYTES: Gauge = Gauge::new();
/// Live `Run`s (held by ingest, the coordinator, the comparator or a chained child).
pub static LIVE_RUNS: Gauge = Gauge::new();
/// Agave grouped-notification frames: queued from the tee to the comparator.
pub static FRAME_QUEUE_BYTES: Gauge = Gauge::new();
/// FINAL records queued from the coordinator to the comparator.
pub static CMP_QUEUE_BYTES: Gauge = Gauge::new();
/// Frames and FINAL records the comparator holds while waiting for the other side.
pub static CMP_HELD_BYTES: Gauge = Gauge::new();
/// Entries released by the gate and not yet handed to a run (approximate).
pub static INGEST_PENDING_BYTES: Gauge = Gauge::new();
/// Agave banks the ingest cache holds (count).
pub static BANKS_HELD: Gauge = Gauge::new();
/// Loaded entries in FL's private program cache (count) and their memory (ELF, sections, JIT
/// code; the SVM keeps loaded entries under `MAX_LOADED_ENTRY_COUNT`). Entries seeded from
/// agave's cache share agave's memory, so this over-counts; reported, not part of the cap.
pub static PROGRAM_ENTRIES: Gauge = Gauge::new();
pub static PROGRAM_BYTES: Gauge = Gauge::new();
/// Scheduler change-probability and rebase-miss hints (bytes estimate).
pub static HINT_BYTES: Gauge = Gauge::new();
/// Idle program-input buffers and PDA on-curve caches of FL's executor threads (`vm_opts`;
/// bounded per thread by the runtime: ≤ 9 × 4 MiB buffers, one fixed-size cache).
pub static VM_POOL_BYTES: Gauge = Gauge::new();
/// Delta-rebase prediction records the coordinator keeps until the predicted transaction's
/// next incarnation verifies them (record + predicted account data; released with the run).
pub static PRED_BYTES: Gauge = Gauge::new();

/// Full processing results of the commit-mode shadow check (`full_cmp`): agave captures
/// queued from replay to the comparator and both sides held while waiting for the other.
pub static FULL_BYTES: Gauge = Gauge::new();

/// Commit mode: FINAL transactions' processing results waiting for their commit (the
/// committer's queue and jobs handed to commit threads).
pub static COMMIT_PENDING_BYTES: Gauge = Gauge::new();

/// Times the cap made the fast lane disable itself.
pub static CAP_TRIPS: AtomicU64 = AtomicU64::new(0);
/// Times the fast lane re-enabled itself after a cap trip (memory back under the low
/// watermark and the node caught up).
pub static CAP_REENABLES: AtomicU64 = AtomicU64::new(0);
/// After a cap trip, memory must fall below this percentage of the cap before re-enabling.
pub const CAP_LOW_WATER_PCT: i64 = 25;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub overlay: i64,
    pub live_runs: i64,
    pub frame_queue: i64,
    pub cmp_queue: i64,
    pub cmp_held: i64,
    pub ingest_pending: i64,
    pub banks_held: i64,
    pub program_entries: i64,
    pub program: i64,
    pub hints: i64,
    pub pred: i64,
    pub vm_pool: i64,
    pub full: i64,
    pub commit: i64,
    /// Commit boards (agave-side claim tables, `solana_runtime::fast_lane_commit`).
    pub board: i64,
}

impl Snapshot {
    pub fn now() -> Self {
        Self {
            overlay: OVERLAY_BYTES.get(),
            live_runs: LIVE_RUNS.get(),
            frame_queue: FRAME_QUEUE_BYTES.get(),
            cmp_queue: CMP_QUEUE_BYTES.get(),
            cmp_held: CMP_HELD_BYTES.get(),
            ingest_pending: INGEST_PENDING_BYTES.get(),
            banks_held: BANKS_HELD.get(),
            program_entries: PROGRAM_ENTRIES.get(),
            program: PROGRAM_BYTES.get(),
            hints: HINT_BYTES.get(),
            pred: PRED_BYTES.get(),
            vm_pool: VM_POOL_BYTES.get(),
            full: FULL_BYTES.get(),
            commit: COMMIT_PENDING_BYTES.get(),
            board: solana_runtime::fast_lane_commit::BOARD_BYTES
                .load(std::sync::atomic::Ordering::Relaxed) as i64,
        }
    }

    /// Bytes counted against the cap.
    pub fn total_bytes(&self) -> i64 {
        self.overlay
            + self.frame_queue
            + self.cmp_queue
            + self.cmp_held
            + self.ingest_pending
            + self.hints
            + self.pred
            + self.vm_pool
            + self.full
            + self.commit
            + self.board
    }
}

/// Most frozen banks the ingest cache may hold (parents of slots FL may still run).
pub const MAX_BANKS_HELD: usize = 64;

/// Most live runs (each holds an overlay and a bank); more means runs are leaking.
pub const MAX_LIVE_RUNS: i64 = 256;
