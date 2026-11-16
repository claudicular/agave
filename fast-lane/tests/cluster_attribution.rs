#![cfg(feature = "agave-unstable-api")]
//! Attribution of a cluster bank-hash disagreement (`cluster_check`): a duplicate block brings
//! the fast lane back once replay is past the dumped slots; the same block replayed without the
//! fast lane to the cluster's hash makes the poison sticky; a fast signal the cluster
//! contradicts is a false alarm; a descendant of a disagreeing version inherits its verdict.
//!
//! Poison state is process-global, so this file has one test running the cases in order (the
//! sticky case last).

use {
    agave_fast_lane::{
        cluster_check::{self, BlockVersion, EventKind},
        control,
    },
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_runtime::fast_lane_commit,
};

fn version(hash: Hash, parent: (u64, Hash), last_entry: Hash, fl_commits: u32) -> BlockVersion {
    BlockVersion {
        hash,
        parent_slot: parent.0,
        parent_hash: parent.1,
        block_id: Some(last_entry),
        last_entry_hash: last_entry,
        fl_commits,
    }
}

fn freeze(slot: u64, v: BlockVersion) {
    let hash = v.hash;
    cluster_check::on_bank_version(slot, v);
    cluster_check::on_bank_frozen(slot, hash);
}

fn verdicts(slot: u64) -> Vec<&'static str> {
    cluster_check::recent_events()
        .into_iter()
        .filter(|e| e.slot == slot)
        .filter_map(|e| match e.kind {
            EventKind::Attribution { verdict } => Some(verdict),
            _ => None,
        })
        .collect()
}

fn fl_on() -> bool {
    control::is_active()
        && !control::is_poisoned()
        && fast_lane_commit::mode() == fast_lane_commit::MODE_ON
        && fast_lane_commit::fl_live()
}

#[test]
fn test_attribution() {
    control::set_active(true);
    control::set_commit_mode(control::COMMIT_ON);
    assert!(fl_on());
    let base = 70_000u64;
    let root = (base - 1, Hash::new_unique());
    freeze(root.0, version(root.1, (base - 2, Hash::new_unique()), Hash::new_unique(), 0));

    // 1. Fast signal, then the cluster confirms our hash: false alarm, FL back.
    let s = base;
    let ours = Hash::new_unique();
    freeze(s, version(ours, root, Hash::new_unique(), 10));
    let other = Hash::new_unique();
    for _ in 0..4 {
        cluster_check::on_vote(s, other, &Pubkey::new_unique(), 100, 1_000, true, true);
    }
    assert!(control::is_poisoned() && !control::is_poisoned_sticky());
    assert!(!fast_lane_commit::fl_live(), "agave stops waiting for FL at once");
    cluster_check::on_cluster_match(s, ours);
    assert_eq!(verdicts(s), vec!["false_alarm"]);
    assert!(fl_on(), "false alarm lifted");

    // 2. Duplicate block: our version of slot d (block X, FL committed into it) disagrees; the
    //    re-repaired block Y replays to the cluster's hash. FL stays off for d and the descendants
    //    frozen meanwhile, then comes back once replay is past them.
    let d = base + 10;
    let ours_d = Hash::new_unique();
    let cluster_d = Hash::new_unique();
    let block_x = Hash::new_unique();
    freeze(d, version(ours_d, root, block_x, 250));
    // A descendant frozen before the disagreement (it is dumped with d).
    let d1_ours = Hash::new_unique();
    freeze(d + 1, version(d1_ours, (d, ours_d), Hash::new_unique(), 100));
    cluster_check::on_cluster_mismatch(d, Some(ours_d), cluster_d, "duplicate_confirmed");
    assert!(control::is_poisoned() && !control::is_poisoned_sticky());
    assert_eq!(fast_lane_commit::mode(), fast_lane_commit::MODE_OFF);
    assert!(control::poison_reason().unwrap().contains("pending attribution"));
    // The descendant's own disagreement is inherited (no verdict of its own).
    cluster_check::on_cluster_mismatch(d + 1, Some(d1_ours), Hash::new_unique(), "duplicate_confirmed");
    assert_eq!(verdicts(d + 1), vec!["inherited"]);
    // Dumped, repaired: another block for d, replayed (without FL) to the cluster's hash.
    freeze(d, version(cluster_d, root, Hash::new_unique(), 0));
    assert_eq!(verdicts(d), vec!["duplicate_block"]);
    assert!(fast_lane_commit::is_agave_only_slot(d));
    assert!(fast_lane_commit::is_agave_only_slot(d + 1));
    assert!(control::is_poisoned(), "still off until replay is past the dumped slots");
    freeze(d + 1, version(Hash::new_unique(), (d, cluster_d), Hash::new_unique(), 0));
    assert!(control::is_poisoned());
    freeze(d + 2, version(Hash::new_unique(), (d + 1, Hash::new_unique()), Hash::new_unique(), 0));
    assert_eq!(verdicts(d), vec!["duplicate_block", "lifted"]);
    assert!(fl_on(), "FL back after the duplicate block");

    // 3. The same block replayed without FL gives the cluster's hash: FL was wrong; sticky.
    let w = base + 20;
    let ours_w = Hash::new_unique();
    let cluster_w = Hash::new_unique();
    let block = Hash::new_unique();
    freeze(w, version(ours_w, root, block, 300));
    cluster_check::on_cluster_mismatch(w, Some(ours_w), cluster_w, "duplicate_confirmed");
    assert!(control::is_poisoned() && !control::is_poisoned_sticky());
    freeze(w, version(cluster_w, root, block, 0));
    assert_eq!(verdicts(w), vec!["fl_wrong"]);
    assert!(control::is_poisoned_sticky());
    assert!(!control::set_active(true), "enable refused");
    freeze(w + 5, version(Hash::new_unique(), root, Hash::new_unique(), 0));
    assert!(control::is_poisoned(), "a sticky poison is never lifted");
    assert_eq!(fast_lane_commit::mode(), fast_lane_commit::MODE_OFF);
}
