//! Cluster bank-hash check: the safety net for committing fast-lane results into agave's banks
//! ("execute once").
//!
//! If FL ever committed a wrong result, our bank hash for that slot (and every descendant) would
//! differ from the cluster's. Agave already compares our frozen hash with the hash the cluster
//! duplicate-confirms (more than 52% of stake voted it) for every slot above the root
//! (`core/src/repair/cluster_slot_state_verifier.rs`). On a disagreement it marks our version
//! invalid in fork choice and schedules the slot for dump and repair: `ReplayStage::
//! dump_then_repair_correct_slots` removes the slot and its descendants from `BankForks` and the
//! blockstore, repair fetches the shreds again and replay re-executes them. The verifier calls
//! this module at those decision points:
//!
//! - [`on_cluster_mismatch`] poisons the fast lane (sticky until restart) with a reason, logs
//!   loudly and emits `fast_lane_cluster_mismatch`. It runs on the replay thread before the dump,
//!   so the re-replay of the dumped slots, which starts only after repair, runs with FL off.
//! - [`on_bank_frozen`] / [`on_cluster_match`] record per-slot freeze and confirmation times for
//!   the coverage and lag metrics (`fast_lane_cluster_check`, every 10 s).
//! - [`on_vote`] (vote listener, gossip and replayed votes) is an earlier, independent signal and
//!   a measurement. Every tower vote carries the bank hash of its last voted slot. Votes from at
//!   least `AGAVE_FL_CLUSTER_FAST_PCT` percent of stake (default 33, 0 disables) for another hash
//!   than our frozen hash poison FL without waiting for duplicate confirmation. And when our hash
//!   is wrong, every replayed vote on that slot fails its SlotHashes check, so only gossip votes
//!   can duplicate-confirm the cluster's hash: the gossip-only agreement times reported here are
//!   the detection lag in that case.
//!
//! Poisoning does not repair anything by itself; agave's dump-and-repair does. Poisoning only
//! makes sure FL's results are no longer used. The execute-once commit path must consult
//! [`crate::control::is_active`] when it decides to use FL results for a bank.
//!
//! **Attribution.** A disagreement disables FL *provisionally* ([`control::poison_provisional`])
//! and the verdict comes when agave has dumped the slot and replayed it again without FL
//! ([`on_bank_version`] records every frozen version's block identity: `block_id` and last entry
//! hash, and how many transactions FL committed into it):
//! - the re-replayed block differs from the one we (and FL) executed and its hash is the
//!   cluster's: a **duplicate block**, not FL's fault. FL stays off for the dumped slot and its
//!   descendants (they are marked agave-only) and comes back once a slot above them is frozen;
//! - the same block, re-replayed without FL, gives the cluster's hash: **FL was wrong**; the
//!   poison becomes sticky until restart;
//! - anything else (the re-replay disagrees again, the version is unknown, we marked the slot
//!   dead, no re-replay within 2 min): **can't tell**; sticky.
//! A disagreement on a slot whose parent version was itself found wrong is inherited from the
//! parent's verdict. A fast signal that the cluster then contradicts (it confirms our hash) is
//! lifted as a false alarm.
//!
//! **Alpenglow.** None of the TowerBFT signals above exists once Alpenglow is enabled: votes
//! certify block ids, not bank hashes, the vote listener returns before [`on_vote`], and replay
//! never runs the cluster-slot-state verifier. The reference becomes the bank hash the **leader**
//! put in the block footer, compared by replay at freeze (`freeze_and_verify_bank_hash`). It is
//! the leader's claim, not a stake-weighted confirmation, but every honest validator votes for a
//! block only after the same comparison passed, and detection is at freeze. Replay calls:
//! - [`on_block_footer_match`] for every checked freeze that matched: the slot counts as matched
//!   (source `block_footer`, so [`last_matched_slot`], the output ring's rollback horizon and
//!   the `fast_lane_cluster_check` metrics advance);
//! - [`on_block_footer_mismatch`] when it differs. Upstream marks such a slot dead. If FL
//!   committed into the bank, replay instead clears the bank and replays the slot once without
//!   FL, with FL disabled provisionally; that replay's footer check decides:
//!   - it matches, on the same block: **`fl_wrong`**, sticky until restart;
//!   - it still differs, on the same block, with FL's hash: **`leader_footer`** (the leader's
//!     claim or agave's own replay is at fault, not FL): the provisional poison is lifted and
//!     replay marks the slot dead as upstream would;
//!   - it still differs, with another hash than FL's: FL changed the bank, **`fl_wrong`**
//!     (sticky), and the slot is marked dead;
//!   - the replay's bank is cleared ([`on_bank_cleared`]: switch, UpdateParent) or dies
//!     ([`on_replay_dead`]) first, a different block is replayed, or nothing within 2 min:
//!     **`unresolved`**, sticky.
//!
//!   A mismatch on a bank FL committed nothing into is recorded (`not_fl`) and leaves FL as it
//!   is. The vote-based fast signal and the gossip-lag metrics are TowerBFT-only: the report is
//!   labelled `consensus=alpenglow` once a footer was checked and leaves them out.

use {
    crate::{
        control::{self, TapStats},
        tees::AgaveEvent,
    },
    log::{error, info, warn},
    parking_lot::Mutex,
    solana_clock::{BankId, Slot},
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::{
        collections::{BTreeMap, HashMap, HashSet, VecDeque},
        sync::OnceLock,
        time::{Duration, Instant},
    },
};

/// Stake fraction (percent) at which agave duplicate-confirms a slot (`DUPLICATE_THRESHOLD`).
const DUPLICATE_PCT: u64 = 52;
/// Default for `AGAVE_FL_CLUSTER_FAST_PCT`.
const FAST_PCT_DEFAULT: u64 = 33;
/// Slot records kept below the highest slot seen; older ones are dropped (counted `unchecked`
/// if they never got a verdict).
const KEEP_SLOTS: Slot = 512;
/// Votes are tracked only for slots within this distance of the highest frozen slot.
const VOTE_WINDOW: Slot = 64;
const REPORT_EVERY: Duration = Duration::from_secs(10);
const MAX_EVENTS: usize = 1024;
/// Mismatches remembered to recognise the replayed slot's match (FIFO).
const MAX_MISMATCHED: usize = 1024;

fn fast_pct() -> u64 {
    static PCT: OnceLock<u64> = OnceLock::new();
    *PCT.get_or_init(|| {
        std::env::var("AGAVE_FL_CLUSTER_FAST_PCT")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|p| *p <= 100)
            .unwrap_or(FAST_PCT_DEFAULT)
    })
}

fn at_least_pct(stake: u64, total: u64, pct: u64) -> bool {
    total > 0 && (stake as u128) * 100 >= (pct as u128) * (total as u128)
}

/// What the cluster check concluded, kept for tests and debugging ([`recent_events`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// The cluster confirmed our frozen hash. `recovered`: after an earlier mismatch on the same
    /// slot and cluster hash (i.e. the dumped slot was replayed to the cluster's hash).
    Match { recovered: bool },
    /// The cluster's hash differs from our frozen hash (`our_hash` is `None` when we marked the
    /// slot dead). FL was poisoned.
    Mismatch { source: &'static str },
    /// Votes from at least the fast threshold of stake carry another hash than ours. FL was
    /// poisoned.
    FastSignal { stake_pct: u64 },
    /// A disagreement was attributed (see the module docs): `duplicate_block`, `fl_wrong`,
    /// `unresolved`, `inherited`, `false_alarm`, `lifted` (FL back after a duplicate block),
    /// and for Alpenglow block footers `leader_footer` and `not_fl`.
    Attribution { verdict: &'static str },
}

/// One frozen version of a slot (recorded by replay at freeze, [`on_bank_version`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockVersion {
    pub hash: Hash,
    pub parent_slot: Slot,
    pub parent_hash: Hash,
    /// Merkle root of the block's last FEC set (`Bank::block_id`), when known.
    pub block_id: Option<Hash>,
    /// Hash of the block's last entry (the blockhash the slot registers).
    pub last_entry_hash: Hash,
    /// Transactions the fast lane committed into this bank (execute once).
    pub fl_commits: u32,
}

impl BlockVersion {
    /// The same block: the same entries (the last entry's PoH hash chains every entry and
    /// transaction of the slot), whatever the resulting bank hash. The bank's state depends on
    /// the entries only, not on how they were shredded (`block_id` is logged, not compared), so a
    /// re-repaired version with the same entries is the same block.
    pub fn same_block(&self, other: &BlockVersion) -> bool {
        self.last_entry_hash == other.last_entry_hash
    }
}

/// An open disagreement (FL provisionally disabled) waiting for its verdict.
struct Attribution {
    slot: Slot,
    token: u64,
    first: BlockVersion,
    /// `None` while only the fast signal fired (no dump yet).
    cluster_hash: Option<Hash>,
    source: &'static str,
    /// Highest slot frozen when the disagreement was found: every dumped descendant is at or
    /// below it.
    horizon: Slot,
    created: Instant,
    /// Duplicate block: FL comes back once a slot above this is frozen.
    lift_above: Option<Slot>,
    /// Alpenglow block-footer disagreement: the bank FL committed into (`first`). The verdict
    /// comes from the footer check of the slot's next bank over the same parent, which replay
    /// creates and replays without FL.
    footer_bank: Option<BankId>,
}

impl Attribution {
    /// `bank_id` (over `parent`) is the replay without FL that decides this footer
    /// disagreement: another bank of the slot over the same parent version.
    fn is_footer_rereplay(&self, slot: Slot, bank_id: BankId, parent: (Slot, Hash)) -> bool {
        self.slot == slot
            && self.footer_bank.is_some_and(|first| first != bank_id)
            && (self.first.parent_slot, self.first.parent_hash) == parent
    }
}

/// Open attributions older than this without a verdict become sticky.
const ATTRIBUTION_TIMEOUT: Duration = Duration::from_secs(120);
/// Slots marked agave-only after a duplicate block (the dumped slot and its descendants).
const MAX_AGAVE_ONLY_SPAN: Slot = 256;
/// Versions of known-wrong frozen banks remembered (to recognise inherited disagreements).
const MAX_BAD: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub slot: Slot,
    pub kind: EventKind,
    pub our_hash: Option<Hash>,
    pub cluster_hash: Hash,
    /// Milliseconds from our freeze to this event, if the freeze was seen.
    pub lag_ms: Option<u64>,
    pub fl_was_active: bool,
}

struct HashVotes {
    hash: Hash,
    /// Stake of distinct voters seen by any path (gossip or replay).
    stake: u64,
    /// Stake of distinct voters seen in gossip.
    gossip_stake: u64,
    /// Voters seen in gossip (first 8 bytes of the vote pubkey; collisions only blur a metric).
    gossip_voters: HashSet<u64>,
}

#[derive(Default)]
struct SlotRec {
    our_hash: Option<Hash>,
    frozen_at: Option<Instant>,
    total_stake: u64,
    votes: Vec<HashVotes>,
    verdict: bool,
    /// Gossip-only stake for our hash crossed 1/3, 52%, 2/3 at these times.
    gossip_cross: [Option<Instant>; 3],
    fast_fired: bool,
}

#[derive(Default)]
struct Stats {
    frozen: u64,
    matched: u64,
    matched_at_freeze: u64,
    mismatched: u64,
    dead_confirmed: u64,
    recovered: u64,
    fast_fired: u64,
    unchecked: u64,
    votes: u64,
    gossip_votes: u64,
    /// Freeze -> cluster confirmation (ms), this interval.
    lag_ms: Vec<u64>,
    /// Freeze -> gossip-only stake for our hash >= 52% (ms, 0 if before freeze), this interval.
    gossip52_ms: Vec<u64>,
    /// Slots dropped with a verdict whose gossip-only stake for our hash never reached 52%.
    gossip52_never: u64,
    gossip52_reached: u64,
    attrib_duplicate: u64,
    attrib_fl_wrong: u64,
    attrib_unresolved: u64,
    attrib_inherited: u64,
    attrib_false_alarm: u64,
    lifted: u64,
    /// Alpenglow: freezes checked against the leader's block footer, and their outcome.
    footer_matched: u64,
    footer_mismatched: u64,
    /// Footer mismatches on a bank FL committed into: replayed once without FL.
    footer_retries: u64,
    attrib_leader_footer: u64,
    attrib_not_fl: u64,
    /// Banks replay cleared (UpdateParent, abandoned bank, switch, replay without FL).
    cleared: u64,
}

struct State {
    slots: BTreeMap<Slot, SlotRec>,
    max_slot: Slot,
    /// (slot, cluster hash) of mismatches, to recognise the replayed slot's match.
    mismatched: VecDeque<(Slot, Hash)>,
    events: VecDeque<Event>,
    interval: Stats,
    total: Stats,
    last_report: Instant,
    /// Frozen versions per slot (block identity), for attribution.
    versions: BTreeMap<Slot, Vec<BlockVersion>>,
    /// Frozen versions found wrong.
    bad: VecDeque<(Slot, Hash)>,
    attributions: Vec<Attribution>,
    /// A block footer was checked: Alpenglow is enabled (the TowerBFT signals are dead).
    alpenglow: bool,
}

impl State {
    fn new() -> Self {
        Self {
            slots: BTreeMap::new(),
            max_slot: 0,
            mismatched: VecDeque::new(),
            events: VecDeque::new(),
            interval: Stats::default(),
            total: Stats::default(),
            last_report: Instant::now(),
            versions: BTreeMap::new(),
            bad: VecDeque::new(),
            attributions: Vec::new(),
            alpenglow: false,
        }
    }

    fn is_bad(&self, slot: Slot, hash: Hash) -> bool {
        self.bad.contains(&(slot, hash))
    }

    fn attribution_event(&mut self, slot: Slot, verdict: &'static str, first: Option<&BlockVersion>,
                         cluster_hash: Hash, source: &'static str, detail: &str) {
        for stats in [&mut self.interval, &mut self.total] {
            match verdict {
                "duplicate_block" => stats.attrib_duplicate += 1,
                "fl_wrong" => stats.attrib_fl_wrong += 1,
                "inherited" => stats.attrib_inherited += 1,
                "false_alarm" => stats.attrib_false_alarm += 1,
                "lifted" => stats.lifted += 1,
                "leader_footer" => stats.attrib_leader_footer += 1,
                "not_fl" => stats.attrib_not_fl += 1,
                _ => stats.attrib_unresolved += 1,
            }
        }
        let fl_commits = first.map(|f| f.fl_commits).unwrap_or(0);
        warn!(
            "fast lane: cluster check attribution for slot {slot}: {verdict} ({detail}; source {source}, cluster hash {cluster_hash}, FL commits in our version {fl_commits})"
        );
        solana_metrics::datapoint_warn!(
            "fast_lane_cluster_attribution",
            ("slot", slot as i64, i64),
            ("verdict", verdict, String),
            ("source", source, String),
            ("cluster_hash", cluster_hash.to_string(), String),
            (
                "our_hash",
                first.map(|f| f.hash.to_string()).unwrap_or_default(),
                String
            ),
            ("fl_commits", i64::from(fl_commits), i64),
            ("detail", detail, String),
        );
        self.push_event(Event {
            slot,
            kind: EventKind::Attribution { verdict },
            our_hash: first.map(|f| f.hash),
            cluster_hash,
            lag_ms: None,
            fl_was_active: control::is_active(),
        });
    }

    /// Our frozen version of `slot` disagrees with the cluster (`cluster_hash`; `None` for the
    /// fast signal): disable FL provisionally and open an attribution, or decide at once.
    fn disagree(
        &mut self,
        slot: Slot,
        our_hash: Option<Hash>,
        cluster_hash: Option<Hash>,
        source: &'static str,
        reason: &str,
    ) {
        if let Some(a) = self
            .attributions
            .iter_mut()
            .find(|a| a.slot == slot && a.lift_above.is_none())
        {
            // E.g. the fast signal, then the cluster's confirmation.
            if a.cluster_hash.is_none() {
                a.cluster_hash = cluster_hash;
                a.source = source;
            }
            return;
        }
        let Some(ours) = our_hash else {
            control::poison(&format!("{reason} (cannot attribute: we marked the slot dead)"));
            self.attribution_event(slot, "unresolved", None, cluster_hash.unwrap_or_default(),
                source, "slot marked dead");
            return;
        };
        if self.bad.len() >= MAX_BAD {
            self.bad.pop_front();
        }
        if !self.is_bad(slot, ours) {
            self.bad.push_back((slot, ours));
        }
        let first = self
            .versions
            .get(&slot)
            .and_then(|vs| vs.iter().rev().find(|v| v.hash == ours))
            .cloned();
        let Some(first) = first else {
            control::poison(&format!("{reason} (cannot attribute: our version is unknown)"));
            self.attribution_event(slot, "unresolved", None, cluster_hash.unwrap_or_default(),
                source, "our frozen version was not recorded");
            return;
        };
        if self.is_bad(first.parent_slot, first.parent_hash) {
            // Our parent version was already found wrong: this follows from it.
            self.attribution_event(slot, "inherited", Some(&first),
                cluster_hash.unwrap_or_default(), source, "parent version already disagreed");
            return;
        }
        let token = control::poison_provisional(reason);
        self.attributions.push(Attribution {
            slot,
            token,
            first,
            cluster_hash,
            source,
            horizon: self.max_slot.max(slot),
            created: Instant::now(),
            lift_above: None,
            footer_bank: None,
        });
    }

    /// A new version of `slot` was frozen: decide the open attribution of the slot, if any.
    fn resolve(&mut self, slot: Slot, version: &BlockVersion) {
        let mut decided = Vec::new();
        for (i, a) in self.attributions.iter().enumerate() {
            // Footer disagreements are decided by the replay's own footer check.
            if a.slot != slot
                || a.lift_above.is_some()
                || a.footer_bank.is_some()
                || version.hash == a.first.hash
            {
                continue;
            }
            let Some(cluster) = a.cluster_hash else {
                continue;
            };
            decided.push((i, cluster));
        }
        for (i, cluster) in decided.into_iter().rev() {
            let a = &self.attributions[i];
            let (token, first, source, horizon) = (a.token, a.first.clone(), a.source, a.horizon);
            let same_block = first.same_block(version);
            if self.is_bad(first.parent_slot, first.parent_hash) {
                // The parent turned out wrong meanwhile: this slot's disagreement follows.
                self.attributions.remove(i);
                control::lift_provisional(token, "inherited from the parent's disagreement");
                self.attribution_event(slot, "inherited", Some(&first), cluster, source,
                    "parent version disagreed");
            } else if version.hash == cluster && !same_block {
                let lift_above = horizon.max(slot);
                for s in slot..=lift_above.min(slot + MAX_AGAVE_ONLY_SPAN) {
                    solana_runtime::fast_lane_commit::mark_agave_only(s);
                }
                self.attributions[i].lift_above = Some(lift_above);
                self.attribution_event(slot, "duplicate_block", Some(&first), cluster, source,
                    &format!("re-repaired block differs from the one executed (last entry {} vs {}); FL off until a slot above {lift_above} is frozen",
                    version.last_entry_hash, first.last_entry_hash));
            } else if version.hash == cluster {
                self.attributions.remove(i);
                control::make_sticky(token, &format!("fast lane result was wrong in slot {slot}: the same block replayed without FL gives the cluster's hash {cluster}"));
                self.attribution_event(slot, "fl_wrong", Some(&first), cluster, source,
                    "same block, replay without FL gives the cluster's hash");
            } else {
                self.attributions.remove(i);
                control::make_sticky(token, &format!("cannot attribute the disagreement in slot {slot}: replayed again with hash {}, cluster {cluster}", version.hash));
                self.attribution_event(slot, "unresolved", Some(&first), cluster, source,
                    &format!("replay without FL gives {}, not the cluster's hash", version.hash));
            }
        }
    }

    /// A slot was frozen: bring FL back after duplicate blocks it replayed past; time out
    /// attributions without a verdict.
    fn lift_passed(&mut self, frozen_slot: Slot) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.attributions.len() {
            let a = &self.attributions[i];
            match a.lift_above {
                Some(above) if frozen_slot > above => {
                    let a = self.attributions.remove(i);
                    let back = control::lift_provisional(
                        a.token,
                        &format!("duplicate block at slot {}, replayed past {above}", a.slot),
                    );
                    self.attribution_event(a.slot, "lifted", Some(&a.first),
                        a.cluster_hash.unwrap_or_default(), a.source,
                        &format!("slot {frozen_slot} frozen; fast lane back: {back}"));
                }
                None if now.saturating_duration_since(a.created) > ATTRIBUTION_TIMEOUT => {
                    let a = self.attributions.remove(i);
                    control::make_sticky(a.token, &format!("cannot attribute the disagreement in slot {}: no replay without FL within {:?}", a.slot,
                        ATTRIBUTION_TIMEOUT));
                    self.attribution_event(a.slot, "unresolved", Some(&a.first),
                        a.cluster_hash.unwrap_or_default(), a.source, "timed out");
                }
                _ => i += 1,
            }
        }
    }

    fn push_event(&mut self, event: Event) {
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    fn lag_ms(&self, slot: Slot, now: Instant) -> Option<u64> {
        self.slots
            .get(&slot)
            .and_then(|r| r.frozen_at)
            .map(|t| now.saturating_duration_since(t).as_millis() as u64)
    }

    fn prune(&mut self) {
        let floor = self.max_slot.saturating_sub(KEEP_SLOTS);
        while let Some((&slot, _)) = self.slots.first_key_value() {
            if slot >= floor {
                break;
            }
            let (_, rec) = self.slots.pop_first().unwrap();
            self.finish(&rec);
        }
        while let Some((&slot, _)) = self.versions.first_key_value() {
            if slot >= floor {
                break;
            }
            self.versions.pop_first();
        }
        // Vote detail is only needed near the tip.
        let vote_floor = self.max_slot.saturating_sub(VOTE_WINDOW);
        for (_, rec) in self.slots.range_mut(..vote_floor) {
            if !rec.votes.is_empty() {
                rec.votes = Vec::new();
            }
        }
    }

    /// A record leaves the window: account for coverage.
    fn finish(&mut self, rec: &SlotRec) {
        if rec.our_hash.is_none() {
            return;
        }
        for stats in [&mut self.interval, &mut self.total] {
            if !rec.verdict {
                stats.unchecked += 1;
            } else if rec.gossip_cross[1].is_some() {
                stats.gossip52_reached += 1;
            } else {
                stats.gossip52_never += 1;
            }
        }
    }

    /// Votes from other hashes than ours reached the fast threshold?
    fn check_fast(&mut self, slot: Slot, now: Instant) {
        let pct = fast_pct();
        let Some(rec) = self.slots.get_mut(&slot) else {
            return;
        };
        let Some(ours) = rec.our_hash else {
            return;
        };
        if pct == 0 || rec.fast_fired || rec.total_stake == 0 {
            return;
        }
        let (other, other_hash) = rec.votes.iter().filter(|v| v.hash != ours).fold(
            (0u64, None),
            |(sum, top): (u64, Option<(u64, Hash)>), v| {
                let top = match top {
                    Some((s, h)) if s >= v.stake => Some((s, h)),
                    _ => Some((v.stake, v.hash)),
                };
                (sum.saturating_add(v.stake), top)
            },
        );
        if !at_least_pct(other, rec.total_stake, pct) {
            return;
        }
        rec.fast_fired = true;
        let stake_pct = (other as u128 * 100 / rec.total_stake as u128) as u64;
        let cluster_hash = other_hash.map(|(_, h)| h).unwrap_or_default();
        let lag_ms = rec
            .frozen_at
            .map(|t| now.saturating_duration_since(t).as_millis() as u64);
        for stats in [&mut self.interval, &mut self.total] {
            stats.fast_fired += 1;
        }
        let fl_was_active = control::is_active();
        let reason = format!(
            "votes from {stake_pct}% of stake carry hash {cluster_hash} for slot {slot}, our \
             frozen hash is {ours}"
        );
        error!(
            "fast lane: CLUSTER VOTE HASH DISAGREEMENT: {reason}; fast lane was {}; disabling it \
             until the disagreement is attributed (agave dumps and repairs the slot once the \
             cluster duplicate-confirms its hash)",
            if fl_was_active { "active" } else { "inactive" }
        );
        solana_metrics::datapoint_error!(
            "fast_lane_cluster_vote_disagreement",
            ("slot", slot as i64, i64),
            ("our_hash", ours.to_string(), String),
            ("cluster_hash", cluster_hash.to_string(), String),
            ("stake_pct", stake_pct as i64, i64),
            ("fl_was_active", fl_was_active, bool),
        );
        self.disagree(slot, Some(ours), None, "fast_vote", &reason);
        self.push_event(Event {
            slot,
            kind: EventKind::FastSignal { stake_pct },
            our_hash: Some(ours),
            cluster_hash,
            lag_ms,
            fl_was_active,
        });
    }

    fn maybe_report(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_report) < REPORT_EVERY {
            return;
        }
        self.last_report = now;
        let i = std::mem::take(&mut self.interval);
        let t = &self.total;
        let (lag50, lag90, lag99, lagmax) = percentiles(&i.lag_ms);
        let (g50, g90, g99, gmax) = percentiles(&i.gossip52_ms);
        let pending = self
            .slots
            .values()
            .filter(|r| r.our_hash.is_some() && !r.verdict)
            .count();
        let poisoned = control::is_poisoned();
        if self.alpenglow {
            self.report_alpenglow(&i, pending, poisoned);
            return;
        }
        info!(
            "fast_lane_cluster_check consensus=towerbft frozen={} matched={} at_freeze={} \
             mismatched={} \
             dead_confirmed={} recovered={} fast_fired={} unchecked={} pending={pending} \
             lag_ms_p50={lag50} p90={lag90} p99={lag99} max={lagmax} gossip52_ms_p50={g50} \
             p90={g90} p99={g99} max={gmax} gossip52_reached={} gossip52_never={} votes={} \
             gossip_votes={} total_matched={} total_mismatched={} total_unchecked={} \
             poisoned={poisoned} sticky={} attrib_duplicate={} attrib_fl_wrong={} \
             attrib_unresolved={} attrib_inherited={} attrib_false_alarm={} lifted={} \
             open_attributions={}",
            i.frozen,
            i.matched,
            i.matched_at_freeze,
            i.mismatched,
            i.dead_confirmed,
            i.recovered,
            i.fast_fired,
            i.unchecked,
            i.gossip52_reached,
            i.gossip52_never,
            i.votes,
            i.gossip_votes,
            t.matched,
            t.mismatched,
            t.unchecked,
            control::is_poisoned_sticky(),
            t.attrib_duplicate,
            t.attrib_fl_wrong,
            t.attrib_unresolved,
            t.attrib_inherited,
            t.attrib_false_alarm,
            t.lifted,
            self.attributions.len(),
        );
        solana_metrics::datapoint_info!(
            "fast_lane_cluster_check",
            ("consensus", "towerbft", String),
            ("attrib_duplicate", t.attrib_duplicate as i64, i64),
            ("attrib_fl_wrong", t.attrib_fl_wrong as i64, i64),
            ("attrib_unresolved", t.attrib_unresolved as i64, i64),
            ("lifted", t.lifted as i64, i64),
            ("open_attributions", self.attributions.len() as i64, i64),
            ("frozen", i.frozen as i64, i64),
            ("matched", i.matched as i64, i64),
            ("matched_at_freeze", i.matched_at_freeze as i64, i64),
            ("mismatched", i.mismatched as i64, i64),
            ("dead_confirmed", i.dead_confirmed as i64, i64),
            ("recovered", i.recovered as i64, i64),
            ("fast_fired", i.fast_fired as i64, i64),
            ("unchecked", i.unchecked as i64, i64),
            ("pending", pending as i64, i64),
            ("lag_ms_p50", lag50 as i64, i64),
            ("lag_ms_p90", lag90 as i64, i64),
            ("lag_ms_p99", lag99 as i64, i64),
            ("lag_ms_max", lagmax as i64, i64),
            ("gossip52_ms_p50", g50 as i64, i64),
            ("gossip52_ms_p90", g90 as i64, i64),
            ("gossip52_ms_p99", g99 as i64, i64),
            ("gossip52_ms_max", gmax as i64, i64),
            ("gossip52_reached", i.gossip52_reached as i64, i64),
            ("gossip52_never", i.gossip52_never as i64, i64),
            ("votes", i.votes as i64, i64),
            ("gossip_votes", i.gossip_votes as i64, i64),
            ("total_matched", t.matched as i64, i64),
            ("total_mismatched", t.mismatched as i64, i64),
            ("total_unchecked", t.unchecked as i64, i64),
            ("poisoned", poisoned, bool),
        );
    }

    /// Alpenglow: the reference is the leader's block footer, checked at freeze. The vote-based
    /// fast signal, the freeze-to-confirmation lag and the gossip-only measurements do not exist
    /// (TowerBFT only) and are left out rather than reported as zeros.
    fn report_alpenglow(&self, i: &Stats, pending: usize, poisoned: bool) {
        let t = &self.total;
        info!(
            "fast_lane_cluster_check consensus=alpenglow reference=block_footer frozen={} \
             matched={} mismatched={} footer_retries={} recovered={} unchecked={} \
             pending={pending} cleared={} total_matched={} total_mismatched={} \
             total_footer_retries={} total_unchecked={} poisoned={poisoned} sticky={} \
             attrib_fl_wrong={} attrib_leader_footer={} attrib_not_fl={} attrib_unresolved={} \
             open_attributions={} last_matched={} (TowerBFT vote/gossip signals: n/a)",
            i.frozen,
            i.footer_matched,
            i.footer_mismatched,
            i.footer_retries,
            i.recovered,
            i.unchecked,
            i.cleared,
            t.footer_matched,
            t.footer_mismatched,
            t.footer_retries,
            t.unchecked,
            control::is_poisoned_sticky(),
            t.attrib_fl_wrong,
            t.attrib_leader_footer,
            t.attrib_not_fl,
            t.attrib_unresolved,
            self.attributions.len(),
            last_matched_slot(),
        );
        solana_metrics::datapoint_info!(
            "fast_lane_cluster_check",
            ("consensus", "alpenglow", String),
            ("frozen", i.frozen as i64, i64),
            ("matched", i.footer_matched as i64, i64),
            ("mismatched", i.footer_mismatched as i64, i64),
            ("footer_retries", i.footer_retries as i64, i64),
            ("recovered", i.recovered as i64, i64),
            ("unchecked", i.unchecked as i64, i64),
            ("pending", pending as i64, i64),
            ("cleared", i.cleared as i64, i64),
            ("total_matched", t.footer_matched as i64, i64),
            ("total_mismatched", t.footer_mismatched as i64, i64),
            ("total_footer_retries", t.footer_retries as i64, i64),
            ("total_unchecked", t.unchecked as i64, i64),
            ("attrib_fl_wrong", t.attrib_fl_wrong as i64, i64),
            ("attrib_leader_footer", t.attrib_leader_footer as i64, i64),
            ("attrib_not_fl", t.attrib_not_fl as i64, i64),
            ("attrib_unresolved", t.attrib_unresolved as i64, i64),
            ("open_attributions", self.attributions.len() as i64, i64),
            ("last_matched", last_matched_slot() as i64, i64),
            ("poisoned", poisoned, bool),
            ("sticky", control::is_poisoned_sticky(), bool),
        );
    }
}

/// (p50, p90, p99, max) of `samples`, 0s when empty.
fn percentiles(samples: &[u64]) -> (u64, u64, u64, u64) {
    if samples.is_empty() {
        return (0, 0, 0, 0);
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    let at = |p: usize| s[(p * (s.len() - 1)) / 100];
    (at(50), at(90), at(99), s[s.len() - 1])
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::new()))
}

impl State {
    /// Replay froze `slot` with `hash`.
    fn frozen(&mut self, slot: Slot, hash: Hash, now: Instant) {
        self.max_slot = self.max_slot.max(slot);
        {
            let rec = self.slots.entry(slot).or_default();
            rec.our_hash = Some(hash);
            rec.frozen_at = Some(now);
            rec.verdict = false;
            rec.fast_fired = false;
            rec.gossip_cross = [None; 3];
            // Gossip votes for our hash may have crossed thresholds before the freeze.
            if rec.total_stake > 0
                && let Some(v) = rec.votes.iter().find(|v| v.hash == hash)
            {
                for (i, pct) in [33u64, DUPLICATE_PCT, 67].into_iter().enumerate() {
                    if at_least_pct(v.gossip_stake, rec.total_stake, pct) {
                        rec.gossip_cross[i] = Some(now);
                    }
                }
            }
        }
        for stats in [&mut self.interval, &mut self.total] {
            stats.frozen += 1;
        }
        self.check_fast(slot, now);
        self.prune();
    }

    /// The reference hash for `slot` (the cluster's, or under Alpenglow the leader's footer
    /// claim: `footer`) equals our frozen hash.
    fn matched(&mut self, slot: Slot, hash: Hash, now: Instant, footer: bool) {
        LAST_MATCHED.fetch_max(slot, std::sync::atomic::Ordering::Relaxed);
        let lag_ms = self.lag_ms(slot, now);
        let recovered = match self.mismatched.iter().position(|m| *m == (slot, hash)) {
            Some(i) => {
                self.mismatched.remove(i);
                true
            }
            None => false,
        };
        let gossip52 = {
            let rec = self.slots.entry(slot).or_default();
            let first_verdict = !rec.verdict;
            rec.verdict = true;
            if rec.our_hash.is_none() {
                rec.our_hash = Some(hash);
            }
            first_verdict.then(|| (rec.frozen_at, rec.gossip_cross[1]))
        };
        for stats in [&mut self.interval, &mut self.total] {
            stats.matched += 1;
            if recovered {
                stats.recovered += 1;
            }
            if footer {
                stats.footer_matched += 1;
            } else if lag_ms.is_some_and(|lag| lag < 1) {
                stats.matched_at_freeze += 1;
            }
        }
        // Only the interval keeps samples. A footer is checked at the freeze itself: no lag.
        if !footer {
            if let Some(lag) = lag_ms {
                self.interval.lag_ms.push(lag);
            }
            if let Some((Some(frozen_at), Some(crossed))) = gossip52 {
                self.interval
                    .gossip52_ms
                    .push(crossed.saturating_duration_since(frozen_at).as_millis() as u64);
            }
        }
        if recovered {
            warn!(
                "fast lane: cluster check: slot {slot} replayed to the {} hash {hash} after a \
                 mismatch (recovered)",
                if footer {
                    "block footer's"
                } else {
                    "cluster's"
                }
            );
        }
        // A fast signal the cluster contradicts: it confirmed our own hash.
        if let Some(i) = self
            .attributions
            .iter()
            .position(|a| a.slot == slot && a.cluster_hash.is_none() && a.first.hash == hash)
        {
            let a = self.attributions.remove(i);
            control::lift_provisional(
                a.token,
                &format!("the cluster confirmed our hash for slot {slot}"),
            );
            self.attribution_event(
                slot,
                "false_alarm",
                Some(&a.first),
                hash,
                a.source,
                "the cluster confirmed our hash",
            );
        }
        let fl_was_active = control::is_active();
        self.push_event(Event {
            slot,
            kind: EventKind::Match { recovered },
            our_hash: Some(hash),
            cluster_hash: hash,
            lag_ms,
            fl_was_active,
        });
    }

    /// The footer attribution of `slot` that `bank_id` (over `parent`) decides, if any.
    fn footer_attribution(
        &self,
        slot: Slot,
        bank_id: BankId,
        parent: (Slot, Hash),
    ) -> Option<usize> {
        self.attributions
            .iter()
            .position(|a| a.is_footer_rereplay(slot, bank_id, parent))
    }

    /// Close footer attribution `i` as unresolved: sticky.
    fn footer_unresolved(&mut self, i: usize, detail: &str) {
        let a = self.attributions.remove(i);
        control::make_sticky(
            a.token,
            &format!(
                "cannot attribute the block footer disagreement in slot {}: {detail}",
                a.slot
            ),
        );
        self.attribution_event(
            a.slot,
            "unresolved",
            Some(&a.first),
            a.cluster_hash.unwrap_or_default(),
            a.source,
            detail,
        );
    }
}

/// Replay froze `slot` with `hash` (also after a repair re-replay: a new version).
pub fn on_bank_frozen(slot: Slot, hash: Hash) {
    let now = Instant::now();
    let mut guard = state().lock();
    let st = &mut *guard;
    st.frozen(slot, hash, now);
    st.maybe_report(now);
}

/// Replay froze a version of `slot` (called right after the freeze, before the block-footer
/// verdict and [`on_bank_frozen`], so that a mismatch at freeze always finds its version): its
/// block identity, for attributing a later disagreement, and the verdict of an open TowerBFT
/// one when this is the re-replay of a dumped slot.
pub fn on_bank_version(slot: Slot, version: BlockVersion) {
    let mut guard = state().lock();
    let st = &mut *guard;
    let versions = st.versions.entry(slot).or_default();
    if versions.len() >= 8 {
        versions.remove(0);
    }
    versions.push(version.clone());
    st.resolve(slot, &version);
    st.lift_passed(slot);
}

/// Highest slot whose frozen hash the cluster confirmed (0 before any).
static LAST_MATCHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Highest slot whose frozen hash the cluster confirmed: FL results up to it are known good.
pub fn last_matched_slot() -> Slot {
    LAST_MATCHED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The cluster confirmed `hash` for `slot`, and it equals our frozen hash.
pub fn on_cluster_match(slot: Slot, hash: Hash) {
    let now = Instant::now();
    let mut guard = state().lock();
    let st = &mut *guard;
    st.matched(slot, hash, now, false);
    st.maybe_report(now);
}

/// The cluster's hash for `slot` (duplicate-confirmed, or sampled by the ancestor-hashes
/// service) differs from ours; `our_hash` is `None` when we marked the slot dead. Poisons the
/// fast lane; agave then dumps and repairs the slot.
pub fn on_cluster_mismatch(
    slot: Slot,
    our_hash: Option<Hash>,
    cluster_hash: Hash,
    source: &'static str,
) {
    let now = Instant::now();
    let fl_was_active = control::is_active();
    let reason = match our_hash {
        Some(ours) => format!(
            "cluster {source} hash {cluster_hash} for slot {slot} differs from our frozen hash \
             {ours}"
        ),
        None => {
            format!("cluster {source} hash {cluster_hash} for slot {slot}, which we marked dead")
        }
    };
    error!(
        "fast lane: CLUSTER BANK HASH MISMATCH: {reason}; fast lane was {}; disabling it (sticky \
         unless the re-replay shows a duplicate block); agave dumps slot {slot} and its \
         descendants and repairs and replays them",
        if fl_was_active { "active" } else { "inactive" }
    );
    solana_metrics::datapoint_error!(
        "fast_lane_cluster_mismatch",
        ("slot", slot as i64, i64),
        (
            "our_hash",
            our_hash
                .map(|h| h.to_string())
                .unwrap_or_else(|| "dead".into()),
            String
        ),
        ("cluster_hash", cluster_hash.to_string(), String),
        ("source", source, String),
        ("fl_was_active", fl_was_active, bool),
    );
    let mut guard = state().lock();
    let st = &mut *guard;
    st.disagree(slot, our_hash, Some(cluster_hash), source, &reason);
    let lag_ms = st.lag_ms(slot, now);
    if st.mismatched.len() >= MAX_MISMATCHED {
        st.mismatched.pop_front();
    }
    st.mismatched.push_back((slot, cluster_hash));
    if let Some(rec) = st.slots.get_mut(&slot) {
        rec.verdict = true;
    }
    for stats in [&mut st.interval, &mut st.total] {
        if our_hash.is_some() {
            stats.mismatched += 1;
        } else {
            stats.dead_confirmed += 1;
        }
    }
    st.push_event(Event {
        slot,
        kind: EventKind::Mismatch { source },
        our_hash,
        cluster_hash,
        lag_ms,
        fl_was_active,
    });
    st.maybe_report(now);
}

/// Alpenglow: replay froze `version` of `slot` (bank `bank_id`) and its hash equals the bank
/// hash the leader put in the block footer (see the module docs). The slot counts as matched
/// (source `block_footer`). If the bank is the replay without the fast lane of a footer
/// disagreement on the same block, the fast lane's result was wrong: `fl_wrong`, sticky.
pub fn on_block_footer_match(slot: Slot, bank_id: BankId, version: &BlockVersion) {
    let now = Instant::now();
    let mut guard = state().lock();
    let st = &mut *guard;
    st.alpenglow = true;
    if let Some(i) =
        st.footer_attribution(slot, bank_id, (version.parent_slot, version.parent_hash))
    {
        let first = &st.attributions[i].first;
        if first.same_block(version) {
            let a = st.attributions.remove(i);
            let footer = a.cluster_hash.unwrap_or_default();
            control::make_sticky(
                a.token,
                &format!(
                    "fast lane result was wrong in slot {slot}: the same block replayed without \
                     the fast lane gives its block footer's bank hash {footer} (with {} \
                     fast-lane commits our bank hash was {})",
                    a.first.fl_commits, a.first.hash
                ),
            );
            st.attribution_event(
                slot,
                "fl_wrong",
                Some(&a.first),
                footer,
                a.source,
                "same block, replay without FL matches the leader's footer hash",
            );
        } else {
            let detail = format!(
                "another block was replayed without the fast lane (last entry {} vs {})",
                version.last_entry_hash, first.last_entry_hash
            );
            st.footer_unresolved(i, &detail);
        }
    }
    st.frozen(slot, version.hash, now);
    st.matched(slot, version.hash, now, true);
    st.maybe_report(now);
}

/// Alpenglow: the hash of `version` of `slot` (bank `bank_id`) differs from the bank hash the
/// leader put in the block footer, `footer_hash`. Replay marks such a slot dead, except when the
/// fast lane committed into the bank (`version.fl_commits > 0`): it then clears the bank and
/// replays the slot once without the fast lane, which this disables provisionally first. When
/// the bank is that replay, the disagreement is decided here (see the module docs). A mismatch
/// on a bank the fast lane committed nothing into leaves the fast lane as it is (`not_fl`).
pub fn on_block_footer_mismatch(
    slot: Slot,
    bank_id: BankId,
    version: &BlockVersion,
    footer_hash: Hash,
) {
    let now = Instant::now();
    let fl_was_active = control::is_active();
    let (ours, fl_commits) = (version.hash, version.fl_commits);
    let mut guard = state().lock();
    let st = &mut *guard;
    st.alpenglow = true;
    st.frozen(slot, ours, now);
    if let Some(rec) = st.slots.get_mut(&slot) {
        rec.verdict = true;
    }
    if st.mismatched.len() >= MAX_MISMATCHED {
        st.mismatched.pop_front();
    }
    st.mismatched.push_back((slot, footer_hash));
    for stats in [&mut st.interval, &mut st.total] {
        stats.mismatched += 1;
        stats.footer_mismatched += 1;
        if fl_commits > 0 {
            stats.footer_retries += 1;
        }
    }
    solana_metrics::datapoint_error!(
        "fast_lane_cluster_mismatch",
        ("slot", slot as i64, i64),
        ("our_hash", ours.to_string(), String),
        ("cluster_hash", footer_hash.to_string(), String),
        ("source", "block_footer", String),
        ("fl_was_active", fl_was_active, bool),
        ("fl_commits", i64::from(fl_commits), i64),
    );
    st.push_event(Event {
        slot,
        kind: EventKind::Mismatch {
            source: "block_footer",
        },
        our_hash: Some(ours),
        cluster_hash: footer_hash,
        lag_ms: Some(0),
        fl_was_active,
    });
    if let Some(i) =
        st.footer_attribution(slot, bank_id, (version.parent_slot, version.parent_hash))
    {
        let first = &st.attributions[i].first;
        if first.same_block(version) && fl_commits == 0 && first.hash != ours {
            // Agave alone gives another hash for the same block: the fast lane's commits
            // changed the bank (the footer is wrong as well).
            let a = st.attributions.remove(i);
            control::make_sticky(
                a.token,
                &format!(
                    "fast lane result was wrong in slot {slot}: the same block replayed without \
                     the fast lane gives bank hash {ours}, ours with {} fast-lane commits was {} \
                     (the block footer's {footer_hash} matches neither)",
                    a.first.fl_commits, a.first.hash
                ),
            );
            st.attribution_event(
                slot,
                "fl_wrong",
                Some(&a.first),
                footer_hash,
                a.source,
                &format!(
                    "same block, replay without FL gives {ours}, another hash; the footer \
                     matches neither; agave marks the slot dead"
                ),
            );
        } else if first.same_block(version) && fl_commits == 0 {
            // The same block without the fast lane gives the same hash, which disagrees with
            // the footer: the leader's claim (or agave itself) is at fault, not the fast lane.
            let a = st.attributions.remove(i);
            let back = control::lift_provisional(
                a.token,
                &format!(
                    "slot {slot}: replayed without the fast lane, the block still differs from \
                     its footer's bank hash; not the fast lane's fault"
                ),
            );
            st.attribution_event(
                slot,
                "leader_footer",
                Some(&a.first),
                footer_hash,
                a.source,
                &format!(
                    "replay without FL gives {ours}, not the footer's hash either; agave marks \
                     the slot dead; fast lane back: {back}"
                ),
            );
        } else {
            let detail = if fl_commits > 0 {
                format!("its replay committed {fl_commits} fast-lane results again")
            } else {
                format!(
                    "another block was replayed without the fast lane (last entry {} vs {})",
                    version.last_entry_hash, first.last_entry_hash
                )
            };
            st.footer_unresolved(i, &detail);
        }
    } else if fl_commits > 0 {
        let reason = format!(
            "slot {slot}: our bank hash {ours} ({fl_commits} fast-lane commits) differs from the \
             block footer's {footer_hash}"
        );
        error!(
            "fast lane: BLOCK FOOTER BANK HASH MISMATCH: {reason}; fast lane was {}; disabling it \
             pending attribution; replay clears the bank and replays the slot without the fast \
             lane instead of marking it dead",
            if fl_was_active { "active" } else { "inactive" }
        );
        let token = control::poison_provisional(&reason);
        let horizon = st.max_slot.max(slot);
        st.attributions.push(Attribution {
            slot,
            token,
            first: version.clone(),
            cluster_hash: Some(footer_hash),
            source: "block_footer",
            horizon,
            created: now,
            lift_above: None,
            footer_bank: Some(bank_id),
        });
    } else {
        warn!(
            "fast lane: block footer bank hash mismatch in slot {slot} (ours {ours}, footer \
             {footer_hash}) on a bank the fast lane committed nothing into: agave marks the slot \
             dead; the fast lane is not involved"
        );
        st.attribution_event(
            slot,
            "not_fl",
            Some(version),
            footer_hash,
            "block_footer",
            "no fast-lane commits in this bank",
        );
    }
    st.maybe_report(now);
}

/// Replay is clearing bank `bank_id` of `slot` (child of `parent_slot` with `parent_hash`): an
/// Alpenglow UpdateParent restart or abandoned bank, a switch to another block version, a
/// descendant of one of those, or a replay without the fast lane.
/// `solana_runtime::fast_lane_commit::on_bank_clearing` has stopped the bank's board.
/// - A footer disagreement whose replay without the fast lane is cleared before it froze is
///   unresolved (sticky). Clearing the bank the fast lane committed into (the retry) is not.
/// - The fast lane's ingest is told through the existing dead-slot path: it aborts the slot's
///   run, which publishes ROLLBACK for it on the output ring, and skips the slot and its
///   children.
pub fn on_bank_cleared(slot: Slot, bank_id: BankId, parent_slot: Slot, parent_hash: Hash) {
    {
        let mut guard = state().lock();
        let st = &mut *guard;
        for stats in [&mut st.interval, &mut st.total] {
            stats.cleared += 1;
        }
        if let Some(i) = st.footer_attribution(slot, bank_id, (parent_slot, parent_hash)) {
            st.footer_unresolved(
                i,
                &format!(
                    "its replay without the fast lane (bank {bank_id}) was cleared before it \
                     froze"
                ),
            );
        }
    }
    notify_ingest_cleared(slot);
}

/// Replay marks `slot` dead after a replay error it does not retry (bank `bank_id`, child of
/// `parent_slot` with `parent_hash`): a footer disagreement whose replay without the fast lane
/// failed is unresolved (sticky).
pub fn on_replay_dead(slot: Slot, bank_id: BankId, parent_slot: Slot, parent_hash: Hash) {
    let mut guard = state().lock();
    let st = &mut *guard;
    if let Some(i) = st.footer_attribution(slot, bank_id, (parent_slot, parent_hash)) {
        st.footer_unresolved(
            i,
            &format!("its replay without the fast lane (bank {bank_id}) failed; slot marked dead"),
        );
    }
}

/// Tell the fast lane's ingest that replay cleared `slot`, as for a dead slot (`tees`'s
/// slot-status tee): wait-free, only while the fast lane is active.
fn notify_ingest_cleared(slot: Slot) {
    if !control::is_active() {
        return;
    }
    let Some(shared) = crate::shared() else {
        return;
    };
    TapStats::inc(&shared.tap_stats.events);
    if shared.event_tx.try_send(AgaveEvent::Dead { slot }).is_err() {
        TapStats::inc(&shared.tap_stats.event_drops);
    }
}

/// Whether a block footer was checked (Alpenglow is enabled; the TowerBFT signals are dead).
pub fn is_alpenglow() -> bool {
    state().lock().alpenglow
}

/// A vote (gossip or replayed) whose last voted slot is `slot` with bank hash `hash`, from a
/// voter with `stake` of the epoch's `total_stake`. `is_new`: the first time this voter's vote
/// for (`slot`, `hash`) was seen by either path (the vote listener's own de-duplication).
pub fn on_vote(
    slot: Slot,
    hash: Hash,
    voter: &Pubkey,
    stake: u64,
    total_stake: u64,
    is_gossip: bool,
    is_new: bool,
) {
    if stake == 0 || total_stake == 0 {
        return;
    }
    let now = Instant::now();
    let mut guard = state().lock();
    let st = &mut *guard;
    if slot + VOTE_WINDOW < st.max_slot || slot > st.max_slot + VOTE_WINDOW {
        return;
    }
    let voter_key = u64::from_le_bytes(voter.to_bytes()[..8].try_into().unwrap());
    let mut counted_gossip = false;
    let fast_check = {
        let rec = st.slots.entry(slot).or_default();
        rec.total_stake = total_stake;
        let idx = match rec.votes.iter().position(|v| v.hash == hash) {
            Some(i) => i,
            None => {
                rec.votes.push(HashVotes {
                    hash,
                    stake: 0,
                    gossip_stake: 0,
                    gossip_voters: HashSet::new(),
                });
                rec.votes.len() - 1
            }
        };
        let v = &mut rec.votes[idx];
        if is_new {
            v.stake = v.stake.saturating_add(stake);
        }
        if is_gossip && v.gossip_voters.insert(voter_key) {
            v.gossip_stake = v.gossip_stake.saturating_add(stake);
            counted_gossip = true;
        }
        let gossip_stake = v.gossip_stake;
        if counted_gossip && rec.our_hash == Some(hash) {
            for (i, pct) in [33u64, DUPLICATE_PCT, 67].into_iter().enumerate() {
                if rec.gossip_cross[i].is_none() && at_least_pct(gossip_stake, total_stake, pct) {
                    rec.gossip_cross[i] = Some(now);
                }
            }
        }
        is_new && rec.our_hash.is_some() && rec.our_hash != Some(hash)
    };
    for stats in [&mut st.interval, &mut st.total] {
        stats.votes += 1;
        if counted_gossip {
            stats.gossip_votes += 1;
        }
    }
    if fast_check {
        st.check_fast(slot, now);
    }
}

/// The most recent verdicts (at most 1024), oldest first.
pub fn recent_events() -> Vec<Event> {
    state().lock().events.iter().cloned().collect()
}

/// Cumulative (matched, mismatched, dead-confirmed, recovered, unchecked) counts.
pub fn totals() -> (u64, u64, u64, u64, u64) {
    let st = state().lock();
    let t = &st.total;
    (
        t.matched,
        t.mismatched,
        t.dead_confirmed,
        t.recovered,
        t.unchecked,
    )
}

/// (our frozen hash, per voted hash: (stake, gossip-seen stake), total stake).
pub type SlotVotes = (Option<Hash>, HashMap<Hash, (u64, u64)>, u64);

/// Per-slot vote summary kept for recent slots.
pub fn slot_votes(slot: Slot) -> Option<SlotVotes> {
    let st = state().lock();
    st.slots.get(&slot).map(|r| {
        (
            r.our_hash,
            r.votes
                .iter()
                .map(|v| (v.hash, (v.stake, v.gossip_stake)))
                .collect(),
            r.total_stake,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The state is process-global: each test uses its own slots and unique hashes, all within
    // `VOTE_WINDOW` of each other so that no test's records fall out of another's window.

    #[test]
    fn test_match_then_lag_recorded() {
        let h = Hash::new_unique();
        on_bank_frozen(50_280, h);
        on_cluster_match(50_280, h);
        let ev = recent_events()
            .into_iter()
            .rev()
            .find(|e| e.slot == 50_280 && e.cluster_hash == h)
            .unwrap();
        assert_eq!(ev.kind, EventKind::Match { recovered: false });
        assert!(ev.lag_ms.is_some());
    }

    #[test]
    fn test_mismatch_poisons_and_replay_recovers() {
        // Never `set_active(true)` here: other tests in this binary rely on the fast lane being
        // off (`sched::tests::test_tick_releases_runs_when_disabled`).
        let ours = Hash::new_unique();
        let cluster = Hash::new_unique();
        on_bank_frozen(50_290, ours);
        on_cluster_mismatch(50_290, Some(ours), cluster, "duplicate_confirmed");
        assert!(control::is_poisoned());
        assert!(!control::is_active());
        assert!(
            !control::set_active(true),
            "enable must be refused once poisoned"
        );
        assert!(control::poison_reason().is_some());
        let ev = recent_events()
            .into_iter()
            .rev()
            .find(|e| e.slot == 50_290 && e.cluster_hash == cluster)
            .unwrap();
        assert_eq!(
            ev.kind,
            EventKind::Mismatch {
                source: "duplicate_confirmed"
            }
        );
        assert_eq!(ev.our_hash, Some(ours));
        // Dumped and replayed without FL: the new version freezes with the cluster's hash.
        on_bank_frozen(50_290, cluster);
        on_cluster_match(50_290, cluster);
        let ev = recent_events()
            .into_iter()
            .rev()
            .find(|e| e.slot == 50_290 && e.cluster_hash == cluster)
            .unwrap();
        assert_eq!(ev.kind, EventKind::Match { recovered: true });
    }

    #[test]
    fn test_fast_signal_threshold_and_gossip_crossing() {
        let ours = Hash::new_unique();
        let other = Hash::new_unique();
        let slot = 50_300;
        on_bank_frozen(slot, ours);
        let total = 1_000;
        let voters: Vec<Pubkey> = (0..10).map(|_| Pubkey::new_unique()).collect();
        // 60% agree via gossip (crosses 1/3 and 52%, not 2/3); a replay-only duplicate of one
        // voter adds nothing to gossip stake.
        for v in &voters[..6] {
            on_vote(slot, ours, v, 100, total, true, true);
        }
        on_vote(slot, ours, &voters[0], 100, total, false, false);
        let (h, per_hash, t) = slot_votes(slot).unwrap();
        assert_eq!(h, Some(ours));
        assert_eq!(t, total);
        assert_eq!(per_hash[&ours], (600, 600));
        // 30% for another hash: below the 33% default, no fast signal yet.
        for v in &voters[6..9] {
            on_vote(slot, other, v, 100, total, true, true);
        }
        assert!(
            !recent_events()
                .iter()
                .any(|e| e.slot == slot && matches!(e.kind, EventKind::FastSignal { .. }))
        );
        // 40%: fires once, poisons.
        on_vote(slot, other, &voters[9], 100, total, true, true);
        let fired: Vec<_> = recent_events()
            .into_iter()
            .filter(|e| e.slot == slot && matches!(e.kind, EventKind::FastSignal { .. }))
            .collect();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].cluster_hash, other);
        assert_eq!(fired[0].kind, EventKind::FastSignal { stake_pct: 40 });
        assert!(control::is_poisoned());
    }

    #[test]
    fn test_votes_before_freeze_are_judged_at_freeze() {
        let ours = Hash::new_unique();
        let other = Hash::new_unique();
        let slot = 50_310;
        // Make the slot eligible (within the vote window of the highest slot seen).
        on_bank_frozen(slot - 1, Hash::new_unique());
        for _ in 0..4 {
            on_vote(slot, other, &Pubkey::new_unique(), 100, 1_000, true, true);
        }
        assert!(
            !recent_events()
                .iter()
                .any(|e| e.slot == slot && matches!(e.kind, EventKind::FastSignal { .. }))
        );
        on_bank_frozen(slot, ours);
        assert!(
            recent_events()
                .iter()
                .any(|e| e.slot == slot && matches!(e.kind, EventKind::FastSignal { .. }))
        );
    }

    #[test]
    fn test_percentiles() {
        assert_eq!(percentiles(&[]), (0, 0, 0, 0));
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentiles(&v), (50, 90, 99, 100));
    }
}
