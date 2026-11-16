//! Per-slot ordering gate over ledger positions, fed by two sources.
//!
//! A slot's entries are grouped in batches (data sets): contiguous data-shred ranges each
//! ending in a DATA_COMPLETE shred. A position is `(batch_start, entry_index_in_batch)`.
//! Pieces arrive from
//! - the blockstore (agave's completed data sets): whole batches, `entry_offset = 0`,
//!   `final_end = Some(last shred index)`;
//! - the proxy ring v2: whole batches or growing prefixes of one (`entry_offset` = entries
//!   of the batch published before), with `final_end` only on the record that completes it.
//!
//! The gate releases entries strictly in ledger order: a piece extends the frontier when it
//! is in the frontier's batch and starts at or before the frontier's entry offset. Entries
//! delivered twice (both sources, or overlapping ring records) are cross-checked by their
//! transaction signatures; a disagreement means the sources saw different block contents
//! (a wrong guessed batch start, a duplicate block, forged or spliced shreds) and the slot
//! must not be trusted.

use {
    solana_entry::entry::Entry,
    solana_signature::Signature,
    std::{collections::BTreeMap, time::Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Blockstore,
    Ring,
}

pub struct Piece {
    pub batch_start: u32,
    pub entry_offset: u32,
    pub entries: Vec<Entry>,
    /// Last data-shred index of the batch, when this piece completes it.
    pub final_end: Option<u32>,
    pub last_in_slot: bool,
    pub source: Source,
    pub t: Instant,
    pub t_unix_ns: u64,
}

/// Entries released in ledger order, with the arrival stamps of the piece that carried them.
pub struct Released {
    pub entries: Vec<Entry>,
    pub source: Source,
    pub t: Instant,
    pub t_unix_ns: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GateStats {
    pub pieces: u64,
    pub duplicate_entries: u64,
    pub cross_checked_entries: u64,
    pub released_from_ring: u64,
    pub released_from_blockstore: u64,
}

#[derive(Default)]
pub struct Gate {
    next_batch: u32,
    next_offset: u32,
    /// Pending pieces keyed by (batch_start, entry_offset, arrival sequence).
    pending: BTreeMap<(u32, u32, u64), Piece>,
    seq: u64,
    /// Transaction signatures of every released entry, per batch, for cross-checks.
    released_sigs: BTreeMap<u32, Vec<Vec<Signature>>>,
    /// Batch start -> last shred index, for released (completed) batches.
    completed: BTreeMap<u32, u32>,
    complete: bool,
    pub mismatch: bool,
    pub stats: GateStats,
}

fn signatures(entry: &Entry) -> Vec<Signature> {
    entry
        .transactions
        .iter()
        .map(|tx| tx.signatures.first().copied().unwrap_or_default())
        .collect()
}

impl Gate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start at an explicit position (e.g. after an Alpenglow UpdateParent).
    pub fn starting_at(batch_start: u32) -> Self {
        Self {
            next_batch: batch_start,
            ..Self::default()
        }
    }

    /// The slot's last batch has been released.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// The next position the gate is waiting for.
    pub fn frontier(&self) -> (u32, u32) {
        (self.next_batch, self.next_offset)
    }

    /// Whether the batch starting at `batch_start` has been fully released.
    pub fn batch_completed(&self, batch_start: u32) -> bool {
        self.completed.contains_key(&batch_start)
    }

    fn cross_check(&mut self, batch_start: u32, entry_offset: u32, entries: &[Entry]) -> bool {
        let Some(released) = self.released_sigs.get(&batch_start) else {
            return true;
        };
        for (i, entry) in entries.iter().enumerate() {
            let index = entry_offset as usize + i;
            let Some(sigs) = released.get(index) else {
                break;
            };
            self.stats.cross_checked_entries += 1;
            if *sigs != signatures(entry) {
                return false;
            }
        }
        true
    }

    /// Add a piece; returns the entries it (and any pending pieces) released, in order.
    pub fn push(&mut self, piece: Piece) -> Vec<Released> {
        self.stats.pieces += 1;
        if self.mismatch {
            return Vec::new();
        }
        // After completion, pieces are still cross-checked (they are all behind the frontier).
        self.seq += 1;
        self.pending
            .insert((piece.batch_start, piece.entry_offset, self.seq), piece);
        self.drain()
    }

    fn drain(&mut self) -> Vec<Released> {
        let mut out = Vec::new();
        loop {
            // Drop (after cross-checking) pieces entirely behind the frontier.
            let behind: Vec<(u32, u32, u64)> = self
                .pending
                .range(..(self.next_batch, 0, 0))
                .map(|(k, _)| *k)
                .collect();
            for key in behind {
                if let Some(piece) = self.pending.remove(&key) {
                    self.stats.duplicate_entries += piece.entries.len() as u64;
                    if !self.cross_check(piece.batch_start, piece.entry_offset, &piece.entries) {
                        self.mismatch = true;
                        return out;
                    }
                }
            }
            // A piece in the frontier batch starting at or before the frontier offset.
            let candidate = self
                .pending
                .range((self.next_batch, 0, 0)..=(self.next_batch, self.next_offset, u64::MAX))
                .map(|(k, _)| *k)
                .next();
            let Some(key) = candidate else {
                return out;
            };
            let Some(piece) = self.pending.remove(&key) else {
                return out;
            };
            let skip = (self.next_offset - piece.entry_offset) as usize;
            let len = piece.entries.len();
            if skip > len || (skip == len && piece.final_end.is_none()) {
                // Entirely duplicate and not completing the batch.
                self.stats.duplicate_entries += len as u64;
                if !self.cross_check(piece.batch_start, piece.entry_offset, &piece.entries) {
                    self.mismatch = true;
                    return out;
                }
                continue;
            }
            if skip > 0 {
                self.stats.duplicate_entries += skip as u64;
                if !self.cross_check(
                    piece.batch_start,
                    piece.entry_offset,
                    &piece.entries[..skip],
                ) {
                    self.mismatch = true;
                    return out;
                }
            }
            let Piece {
                batch_start,
                entries,
                final_end,
                last_in_slot,
                source,
                t,
                t_unix_ns,
                ..
            } = piece;
            let fresh: Vec<Entry> = entries.into_iter().skip(skip).collect();
            let sigs = self.released_sigs.entry(batch_start).or_default();
            sigs.extend(fresh.iter().map(signatures));
            self.next_offset += fresh.len() as u32;
            match source {
                Source::Ring => self.stats.released_from_ring += fresh.len() as u64,
                Source::Blockstore => self.stats.released_from_blockstore += fresh.len() as u64,
            }
            if !fresh.is_empty() {
                out.push(Released {
                    entries: fresh,
                    source,
                    t,
                    t_unix_ns,
                });
            }
            if let Some(end) = final_end {
                self.completed.insert(batch_start, end);
                self.next_batch = end + 1;
                self.next_offset = 0;
                if last_in_slot {
                    self.complete = true;
                    return out;
                }
            }
        }
    }

    /// Mark the slot complete from outside (blockstore `last_index` reached).
    pub fn set_complete_if_past(&mut self, last_index: u64) {
        if u64::from(self.next_batch) > last_index && self.next_offset == 0 {
            self.complete = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_hash::Hash,
        solana_transaction::versioned::VersionedTransaction,
    };

    fn entry(tag: u8) -> Entry {
        let mut tx = VersionedTransaction::default();
        tx.signatures = vec![Signature::from([tag; 64])];
        Entry {
            num_hashes: 1,
            hash: Hash::default(),
            transactions: vec![tx],
        }
    }

    fn piece(
        batch_start: u32,
        offset: u32,
        tags: &[u8],
        final_end: Option<u32>,
        last: bool,
        source: Source,
    ) -> Piece {
        Piece {
            batch_start,
            entry_offset: offset,
            entries: tags.iter().map(|t| entry(*t)).collect(),
            final_end,
            last_in_slot: last,
            source,
            t: Instant::now(),
            t_unix_ns: 0,
        }
    }

    fn tags(released: &[Released]) -> Vec<u8> {
        released
            .iter()
            .flat_map(|r| r.entries.iter())
            .map(|e| e.transactions[0].signatures[0].as_ref()[0])
            .collect()
    }

    #[test]
    fn test_streamed_ring_then_blockstore() {
        let mut g = Gate::new();
        // Batch 0 (shreds 0..=9): three streamed records.
        assert_eq!(tags(&g.push(piece(0, 0, &[1, 2], None, false, Source::Ring))), [1, 2]);
        assert_eq!(tags(&g.push(piece(0, 2, &[3], None, false, Source::Ring))), [3]);
        // Batch 1 arrives early: waits.
        assert!(g.push(piece(10, 0, &[7, 8], Some(15), false, Source::Ring)).is_empty());
        // The final record of batch 0 releases it and then batch 1.
        assert_eq!(tags(&g.push(piece(0, 3, &[4], Some(9), false, Source::Ring))), [4, 7, 8]);
        // Blockstore copies arrive later: cross-checked, nothing re-released.
        assert!(g.push(piece(0, 0, &[1, 2, 3, 4], Some(9), false, Source::Blockstore)).is_empty());
        assert!(g.push(piece(10, 0, &[7, 8], Some(15), false, Source::Blockstore)).is_empty());
        assert!(!g.mismatch);
        assert_eq!(g.stats.cross_checked_entries, 6);
        assert_eq!(g.frontier(), (16, 0));
    }

    #[test]
    fn test_blockstore_completes_partial_ring_batch() {
        let mut g = Gate::new();
        assert_eq!(tags(&g.push(piece(0, 0, &[1, 2], None, false, Source::Ring))), [1, 2]);
        // The ring lost the rest (lapped); the blockstore's whole batch fills it in.
        let released = g.push(piece(0, 0, &[1, 2, 3, 4], Some(5), true, Source::Blockstore));
        assert_eq!(tags(&released), [3, 4]);
        assert_eq!(released[0].source, Source::Blockstore);
        assert!(g.is_complete());
    }

    #[test]
    fn test_empty_final_marker_advances() {
        let mut g = Gate::new();
        assert_eq!(tags(&g.push(piece(0, 0, &[1, 2], None, false, Source::Ring))), [1, 2]);
        assert!(g.push(piece(3, 0, &[5], Some(4), false, Source::Ring)).is_empty());
        // All of batch 0 went out early; its FINAL marker has no entries.
        assert_eq!(tags(&g.push(piece(0, 2, &[], Some(2), false, Source::Ring))), [5]);
        assert_eq!(g.frontier(), (5, 0));
    }

    #[test]
    fn test_mismatch_detected() {
        let mut g = Gate::new();
        g.push(piece(0, 0, &[1, 2], Some(3), false, Source::Ring));
        g.push(piece(0, 0, &[1, 9], Some(3), false, Source::Blockstore));
        assert!(g.mismatch);
        // A mismatched gate releases nothing more.
        assert!(g.push(piece(4, 0, &[5], Some(5), false, Source::Ring)).is_empty());
    }

    #[test]
    fn test_out_of_order_blockstore_sets() {
        let mut g = Gate::new();
        assert!(g.push(piece(8, 0, &[3], Some(9), false, Source::Blockstore)).is_empty());
        assert!(g.push(piece(4, 0, &[2], Some(7), false, Source::Blockstore)).is_empty());
        assert_eq!(
            tags(&g.push(piece(0, 0, &[1], Some(3), false, Source::Blockstore))),
            [1, 2, 3]
        );
        g.set_complete_if_past(9);
        assert!(g.is_complete());
    }
}
