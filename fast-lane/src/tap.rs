//! The window-service tap: runs on agave's `solWinInsert` thread right after blockstore
//! insertion reports completed data sets (the instant replay is signalled). Wait-free:
//! one relaxed load, a small Vec clone and a `try_send`; drops are counted, never blocked.

use {
    crate::{
        control::{self, TapStats},
        shared,
    },
    solana_ledger::blockstore::CompletedDataSetInfo,
    std::time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Completed data sets observed at one insert, with the observation time.
pub struct TapBatch {
    pub sets: Vec<CompletedDataSetInfo>,
    pub t: Instant,
    pub t_unix_ns: u64,
}

pub fn unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Called by `window_service::run_insert` with the data sets that just completed.
pub fn on_completed_data_sets(sets: &[CompletedDataSetInfo]) {
    if sets.is_empty() || !control::is_active() {
        return;
    }
    let Some(shared) = shared() else {
        return;
    };
    let batch = TapBatch {
        sets: sets.to_vec(),
        t: Instant::now(),
        t_unix_ns: unix_ns(),
    };
    TapStats::inc(&shared.tap_stats.tap_batches);
    if shared.tap_tx.try_send(batch).is_err() {
        TapStats::inc(&shared.tap_stats.tap_drops);
    }
}
