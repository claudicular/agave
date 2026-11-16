#![cfg(feature = "agave-unstable-api")]
//! Alpenglow: attribution of a block-footer bank-hash disagreement (`cluster_check`). The
//! reference is the leader's footer hash, checked by replay at freeze. A mismatch on a bank the
//! fast lane committed into disables it provisionally while replay re-runs the slot without it:
//! - the same block then matches the footer: the fast lane was wrong (`fl_wrong`, sticky);
//! - it still differs with the same hash: the leader's footer (or agave alone) is at fault
//!   (`leader_footer`): lifted; with another hash, the fast lane changed the bank (`fl_wrong`);
//! - that replay is cleared or dies first, or another block is replayed: `unresolved`, sticky.
//!
//! A mismatch on a bank without fast-lane commits does not touch the fast lane (`not_fl`), and a
//! match advances the last matched slot. Poison state is process-global, so this file has one
//! test running the cases in order.

use {
    agave_fast_lane::{
        cluster_check::{self, BlockVersion, EventKind},
        control,
    },
    solana_hash::Hash,
    solana_runtime::fast_lane_commit,
};

fn version(hash: Hash, parent: (u64, Hash), last_entry: Hash, fl_commits: u32) -> BlockVersion {
    BlockVersion {
        hash,
        parent_slot: parent.0,
        parent_hash: parent.1,
        block_id: Some(Hash::new_unique()),
        last_entry_hash: last_entry,
        fl_commits,
    }
}

/// What replay calls at freeze (`process_replay_results`): the version, then the footer verdict.
fn freeze(slot: u64, bank_id: u64, v: &BlockVersion, footer: Hash) {
    cluster_check::on_bank_version(slot, v.clone());
    if v.hash == footer {
        cluster_check::on_block_footer_match(slot, bank_id, v);
    } else {
        cluster_check::on_block_footer_mismatch(slot, bank_id, v, footer);
    }
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

fn fl_on_again() {
    control::unpoison_for_tests();
    assert!(control::set_active(true));
    control::set_commit_mode(control::COMMIT_ON);
    assert!(fl_on());
}

fn pending(slot: u64) -> bool {
    control::open_provisional_reasons()
        .iter()
        .any(|r| r.starts_with(&format!("slot {slot}:")))
}

#[test]
fn test_footer_attribution() {
    fl_on_again();
    let base = 90_000u64;
    let parent = (base - 1, Hash::new_unique());

    // 0. A footer match counts as matched (source block_footer) and advances the rollback
    //    horizon; nothing else happens.
    let s = base;
    let v = version(Hash::new_unique(), parent, Hash::new_unique(), 12);
    freeze(s, 100, &v, v.hash);
    assert!(cluster_check::is_alpenglow());
    assert!(cluster_check::last_matched_slot() >= s);
    assert!(cluster_check::recent_events().iter().any(|e| e.slot == s
        && e.cluster_hash == v.hash
        && e.kind == EventKind::Match { recovered: false }));
    assert!(verdicts(s).is_empty());
    assert!(fl_on());

    // 1. A mismatch on a bank the fast lane committed nothing into: agave marks the slot dead
    //    as upstream; the fast lane is not involved and stays on.
    let s = base + 1;
    let v = version(Hash::new_unique(), parent, Hash::new_unique(), 0);
    freeze(s, 110, &v, Hash::new_unique());
    assert_eq!(verdicts(s), vec!["not_fl"]);
    assert!(fl_on());

    // 2. The leader's footer is wrong and FL right. FL committed into the bank: provisional
    //    poison at once (agave's side of the gate off), the retry's clear of that bank decides
    //    nothing, a bank of the slot over another parent is not the retry, and the replay
    //    without FL (same block, another bank) gives the same hash, which still differs from
    //    the footer: `leader_footer`, FL back.
    let s = base + 2;
    let (block, footer) = (Hash::new_unique(), Hash::new_unique());
    let v = version(Hash::new_unique(), parent, block, 40);
    freeze(s, 120, &v, footer);
    assert!(control::is_poisoned() && !control::is_poisoned_sticky());
    assert_eq!(fast_lane_commit::mode(), fast_lane_commit::MODE_OFF);
    assert!(
        !fast_lane_commit::fl_live(),
        "agave stops waiting for FL at once"
    );
    assert!(pending(s));
    assert!(cluster_check::recent_events().iter().any(|e| e.slot == s
        && e.our_hash == Some(v.hash)
        && e.cluster_hash == footer
        && e.kind
            == EventKind::Mismatch {
                source: "block_footer"
            }));
    cluster_check::on_bank_cleared(s, 120, parent.0, parent.1);
    cluster_check::on_bank_cleared(s, 777, parent.0, Hash::new_unique());
    cluster_check::on_replay_dead(s, 778, parent.0, Hash::new_unique());
    assert!(verdicts(s).is_empty());
    assert!(pending(s));
    let again = version(v.hash, parent, block, 0);
    freeze(s, 121, &again, footer);
    assert_eq!(verdicts(s), vec!["leader_footer"]);
    assert!(!pending(s));
    assert!(fl_on(), "lifted");

    // 3. FL and the footer both wrong: the replay without FL differs from the footer too, but
    //    with another hash than FL's version, so FL changed the bank: `fl_wrong`, sticky.
    let s = base + 3;
    let (block, footer) = (Hash::new_unique(), Hash::new_unique());
    let wrong = version(Hash::new_unique(), parent, block, 3);
    freeze(s, 130, &wrong, footer);
    assert!(control::is_poisoned());
    freeze(
        s,
        131,
        &version(Hash::new_unique(), parent, block, 0),
        footer,
    );
    assert_eq!(verdicts(s), vec!["fl_wrong"]);
    assert!(control::is_poisoned_sticky());
    assert!(!pending(s));
    fl_on_again();

    // 4. The replay without FL is cleared before it froze (a switch, an UpdateParent restart):
    //    `unresolved`, sticky.
    let s = base + 4;
    let footer = Hash::new_unique();
    freeze(
        s,
        140,
        &version(Hash::new_unique(), parent, Hash::new_unique(), 9),
        footer,
    );
    cluster_check::on_bank_cleared(s, 140, parent.0, parent.1);
    assert!(verdicts(s).is_empty());
    cluster_check::on_bank_cleared(s, 141, parent.0, parent.1);
    assert_eq!(verdicts(s), vec!["unresolved"]);
    assert!(control::is_poisoned_sticky());
    assert!(!control::set_active(true), "enable refused");
    fl_on_again();

    // 5. The replay without FL fails (replay error, slot marked dead): `unresolved`.
    let s = base + 5;
    freeze(
        s,
        150,
        &version(Hash::new_unique(), parent, Hash::new_unique(), 9),
        Hash::new_unique(),
    );
    cluster_check::on_replay_dead(s, 151, parent.0, parent.1);
    assert_eq!(verdicts(s), vec!["unresolved"]);
    assert!(control::is_poisoned_sticky());
    fl_on_again();

    // 6. Another block is replayed without FL (switched in meanwhile), even one that matches
    //    its own footer: `unresolved`.
    let s = base + 6;
    let footer = Hash::new_unique();
    freeze(
        s,
        160,
        &version(Hash::new_unique(), parent, Hash::new_unique(), 9),
        footer,
    );
    let other = version(footer, parent, Hash::new_unique(), 0);
    freeze(s, 161, &other, footer);
    assert_eq!(verdicts(s), vec!["unresolved"]);
    assert!(control::is_poisoned_sticky());
    fl_on_again();

    // 7. FL was wrong: the same block replayed without FL matches the footer. `fl_wrong`,
    //    sticky until restart; the slot is recovered (matched, not dead) and the rollback
    //    horizon advances past it.
    let s = base + 7;
    let (block, footer) = (Hash::new_unique(), Hash::new_unique());
    let wrong = version(Hash::new_unique(), parent, block, 250);
    freeze(s, 170, &wrong, footer);
    assert!(control::is_poisoned() && !control::is_poisoned_sticky());
    assert!(cluster_check::last_matched_slot() < s);
    cluster_check::on_bank_cleared(s, 170, parent.0, parent.1);
    freeze(s, 171, &version(footer, parent, block, 0), footer);
    assert_eq!(verdicts(s), vec!["fl_wrong"]);
    assert!(cluster_check::recent_events().iter().any(|e| e.slot == s
        && e.our_hash == Some(wrong.hash)
        && e.kind
            == EventKind::Attribution {
                verdict: "fl_wrong"
            }));
    assert!(cluster_check::recent_events().iter().any(|e| e.slot == s
        && e.cluster_hash == footer
        && e.kind == EventKind::Match { recovered: true }));
    assert!(cluster_check::last_matched_slot() >= s);
    assert!(control::is_poisoned_sticky());
    assert!(!pending(s));
    assert!(!control::set_active(true), "enable refused");
    assert_eq!(fast_lane_commit::mode(), fast_lane_commit::MODE_OFF);
    // A later match does not lift a sticky poison.
    let later = version(Hash::new_unique(), parent, Hash::new_unique(), 0);
    freeze(s + 1, 180, &later, later.hash);
    assert!(control::is_poisoned());
}
