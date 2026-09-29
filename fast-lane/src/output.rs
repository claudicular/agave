//! Phase-3 publisher: turns FINAL transactions into [`out_ring`](crate::out_ring) records.
//!
//! Runs inside the coordinator's sink, i.e. on the coordinator thread, the ring's single
//! producer, at the moment a transaction becomes FINAL. Its cost is measured per published
//! record (owner filter + copy into the ring) and reported by the comparator summary.

use {
    crate::{
        out_ring::{
            AccountRef, FLAG_CHAINED, FLAG_FROM_RING, FLAG_OK, FLAG_SPECULATIVE, FLAG_VOTE,
            KIND_ROLLBACK, KIND_SLOT_BEGIN, KIND_SLOT_END, OutRing, RecordHeader,
        },
        run::TxOutcome,
        sched::{RunId, RunSummary},
    },
    solana_account::ReadableAccount,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{
        collections::{HashMap, VecDeque},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Instant,
    },
};

pub const TOKEN_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Upper bounds (ns) of the write-cost histogram buckets; the last bucket is open.
pub const WRITE_NS_BUCKETS: [u64; 8] = [250, 500, 1_000, 2_000, 5_000, 10_000, 50_000, u64::MAX];

#[derive(Default)]
pub struct OutStats {
    pub tx_records: AtomicU64,
    pub markers: AtomicU64,
    /// FINAL transactions not published (no frame, or no account of a filtered owner).
    pub filtered: AtomicU64,
    pub bytes: AtomicU64,
    /// Records the ring refused (too large even without accounts) and accounts left out.
    pub dropped: AtomicU64,
    pub incomplete: AtomicU64,
    pub write_ns_sum: AtomicU64,
    pub write_ns_max: AtomicU64,
    pub write_ns_hist: [AtomicU64; 8],
}

impl OutStats {
    fn record_write(&self, ns: u64) {
        self.write_ns_sum.fetch_add(ns, Ordering::Relaxed);
        self.write_ns_max.fetch_max(ns, Ordering::Relaxed);
        let bucket = WRITE_NS_BUCKETS
            .iter()
            .position(|&hi| ns <= hi)
            .unwrap_or(WRITE_NS_BUCKETS.len() - 1);
        self.write_ns_hist[bucket].fetch_add(1, Ordering::Relaxed);
    }
}

struct RunInfo {
    slot: Slot,
    parent_slot: Slot,
    chained: bool,
}

pub struct OutPublisher {
    ring: OutRing,
    owners: Vec<Pubkey>,
    pub stats: Arc<OutStats>,
    /// Runs with a SLOT_BEGIN published and no end yet.
    open: HashMap<RunId, RunInfo>,
    /// Recently ended runs (for a ROLLBACK after the end).
    ended: VecDeque<(RunId, RunInfo)>,
}

impl OutPublisher {
    pub fn new(ring: OutRing, owners: Vec<Pubkey>, stats: Arc<OutStats>) -> Self {
        Self {
            ring,
            owners,
            stats,
            open: HashMap::new(),
            ended: VecDeque::new(),
        }
    }

    /// The owners published by default plus `extra`.
    pub fn owner_filter(token: bool, extra: &[Pubkey]) -> Vec<Pubkey> {
        let mut owners = Vec::new();
        if token {
            owners.extend([TOKEN_PROGRAM, TOKEN_2022_PROGRAM]);
        }
        for owner in extra {
            if !owners.contains(owner) {
                owners.push(*owner);
            }
        }
        owners
    }

    /// A transaction became FINAL.
    pub fn on_final(
        &mut self,
        run_id: RunId,
        outcome: &TxOutcome,
        incarnations: u32,
        speculative: bool,
    ) {
        let t0 = Instant::now();
        let Some(frame) = &outcome.frame else {
            self.stats.filtered.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let accounts: Vec<AccountRef> = frame
            .iter()
            .enumerate()
            .filter(|(_, (_, account))| self.owners.contains(account.owner()))
            .map(|(i, (pubkey, account))| AccountRef {
                pubkey,
                account,
                written: outcome.frame_written.get(i).copied().unwrap_or(true),
            })
            .collect();
        if accounts.is_empty() {
            self.stats.filtered.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if !self.open.contains_key(&run_id) {
            let info = RunInfo {
                slot: outcome.slot,
                parent_slot: outcome.parent_slot,
                chained: outcome.chained,
            };
            self.marker(KIND_SLOT_BEGIN, run_id, &info, 0, None);
            self.open.insert(run_id, info);
        }
        let mut flags = 0u16;
        if outcome.status.is_ok() {
            flags |= FLAG_OK;
        }
        if outcome.is_vote {
            flags |= FLAG_VOTE;
        }
        if outcome.chained {
            flags |= FLAG_CHAINED;
        }
        if outcome.from_ring {
            flags |= FLAG_FROM_RING;
        }
        if speculative {
            flags |= FLAG_SPECULATIVE;
        }
        let header = RecordHeader {
            flags,
            slot: outcome.slot,
            parent_slot: outcome.parent_slot,
            tx_ordinal: outcome.ordinal,
            incarnations: incarnations.min(u32::from(u16::MAX)) as u16,
            fork_id: run_id,
            t_source_ns: outcome.t_tap_unix_ns,
            ..RecordHeader::default()
        };
        let signature = <[u8; 64]>::from(outcome.signature);
        let bytes: usize = accounts.iter().map(|a| a.encoded_len()).sum();
        match self.ring.publish_tx(
            header,
            &signature,
            u32::from(outcome.status.is_err()),
            outcome.cu.min(u64::from(u32::MAX)) as u32,
            &accounts,
        ) {
            Some(incomplete) => {
                self.stats.tx_records.fetch_add(1, Ordering::Relaxed);
                self.stats.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
                if incomplete {
                    self.stats.incomplete.fetch_add(1, Ordering::Relaxed);
                }
            }
            None => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.stats.record_write(t0.elapsed().as_nanos() as u64);
    }

    pub fn on_run_end(&mut self, run_id: RunId, summary: &RunSummary) {
        let Some(info) = self.open.remove(&run_id) else {
            return;
        };
        match summary.aborted {
            Some(reason) => self.marker(KIND_ROLLBACK, run_id, &info, 0, Some(reason)),
            None => {
                self.marker(KIND_SLOT_END, run_id, &info, summary.txs as u32, None);
                self.ended.push_back((run_id, info));
                if self.ended.len() > 256 {
                    self.ended.pop_front();
                }
            }
        }
    }

    pub fn on_abort_ended(&mut self, run_id: RunId, reason: &'static str) {
        if let Some(i) = self.ended.iter().position(|(id, _)| *id == run_id) {
            if let Some((_, info)) = self.ended.remove(i) {
                self.marker(KIND_ROLLBACK, run_id, &info, 0, Some(reason));
            }
        }
    }

    pub fn tick(&mut self) {
        self.ring.heartbeat();
    }

    fn marker(&mut self, kind: u16, run_id: RunId, info: &RunInfo, count: u32, reason: Option<&str>) {
        let header = RecordHeader {
            kind,
            flags: if info.chained { FLAG_CHAINED } else { 0 },
            slot: info.slot,
            parent_slot: info.parent_slot,
            tx_ordinal: count,
            fork_id: run_id,
            ..RecordHeader::default()
        };
        if self.ring.publish_marker(header, reason) {
            self.stats.markers.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            out_ring::{OutRingReader, Poll},
            run::OutcomeKind,
        },
        solana_account::AccountSharedData,
        solana_signature::Signature,
        std::time::Instant,
    };

    fn outcome(n_token: usize, n_other: usize, slot: Slot, ordinal: u32) -> TxOutcome {
        let mut frame = Vec::new();
        for _ in 0..n_token {
            frame.push((Pubkey::new_unique(), AccountSharedData::new(2_039_280, 165, &TOKEN_PROGRAM)));
        }
        for _ in 0..n_other {
            frame.push((
                Pubkey::new_unique(),
                AccountSharedData::new(1_000_000, 300, &Pubkey::new_unique()),
            ));
        }
        TxOutcome {
            slot,
            parent_slot: slot - 1,
            ordinal,
            signature: Signature::from([ordinal as u8; 64]),
            is_vote: false,
            programs: Vec::new(),
            kind: OutcomeKind::Executed,
            chained: false,
            status: Ok(()),
            frame_written: vec![true; frame.len()],
            frame: Some(frame),
            cu: 50_000,
            fee: 5000,
            t_tap: Instant::now(),
            t_tap_unix_ns: 1,
            from_ring: true,
            processed: None,
        }
    }

    /// Write cost of one published record on the coordinator (owner filter + copy into the
    /// ring), for a typical swap frame: 5 token accounts + 2 other accounts.
    #[test]
    fn test_publish_cost_and_markers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.ring");
        let ring = OutRing::create(&path, 64 << 20).unwrap();
        let mut reader = OutRingReader::open(&path).unwrap();
        let stats = Arc::new(OutStats::default());
        let mut publisher = OutPublisher::new(ring, OutPublisher::owner_filter(true, &[]), stats.clone());
        let outcomes: Vec<TxOutcome> = (0..1000).map(|i| outcome(5, 2, 10, i)).collect();
        let no_match = outcome(0, 3, 10, 0);
        let iterations = 100_000usize;
        let t0 = Instant::now();
        for i in 0..iterations {
            publisher.on_final(1, &outcomes[i % outcomes.len()], 1, false);
            // Drain as a consumer would, so the ring never laps.
            if i % 64 == 63 {
                while let Poll::Record(_) = reader.poll() {}
            }
        }
        let per_record = t0.elapsed().as_nanos() as f64 / iterations as f64;
        let mean = stats.write_ns_sum.load(Ordering::Relaxed) as f64 / iterations as f64;
        let hist: Vec<u64> = stats.write_ns_hist.iter().map(|h| h.load(Ordering::Relaxed)).collect();
        eprintln!(
            "publish: {per_record:.0} ns/record incl. draining; measured write mean {mean:.0} ns, \
             max {} ns, hist (<=250,500,1k,2k,5k,10k,50k,inf) {hist:?}",
            stats.write_ns_max.load(Ordering::Relaxed)
        );
        assert_eq!(stats.tx_records.load(Ordering::Relaxed), iterations as u64);
        assert!(mean < 20_000.0, "write cost {mean} ns");
        // A frame without a filtered owner is not published.
        publisher.on_final(1, &no_match, 1, false);
        assert_eq!(stats.filtered.load(Ordering::Relaxed), 1);
        // Markers: SLOT_BEGIN once per run, SLOT_END at the end, ROLLBACK after the end.
        while let Poll::Record(_) = reader.poll() {}
        publisher.on_final(2, &outcome(1, 0, 11, 0), 1, true);
        publisher.on_run_end(2, &RunSummary { txs: 1, ..RunSummary::default() });
        publisher.on_abort_ended(2, "dead");
        let kinds: Vec<(u16, u16)> = std::iter::from_fn(|| match reader.poll() {
            Poll::Record(r) => Some((r.header.kind, r.header.flags)),
            _ => None,
        })
        .collect();
        assert_eq!(
            kinds.iter().map(|k| k.0).collect::<Vec<_>>(),
            vec![KIND_SLOT_BEGIN, crate::out_ring::KIND_TX, KIND_SLOT_END, KIND_ROLLBACK]
        );
        assert_ne!(kinds[1].1 & FLAG_SPECULATIVE, 0);
        assert_ne!(kinds[1].1 & FLAG_FROM_RING, 0);
    }
}
