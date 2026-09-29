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

use {
    crate::control,
    log::{error, info, warn},
    parking_lot::Mutex,
    solana_clock::Slot,
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
}

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
            "fast lane: CLUSTER VOTE HASH DISAGREEMENT: {reason}; fast lane was {}; poisoning it \
             (agave dumps and repairs the slot once the cluster duplicate-confirms its hash)",
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
        control::poison(&reason);
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
        info!(
            "fast_lane_cluster_check frozen={} matched={} at_freeze={} mismatched={} \
             dead_confirmed={} recovered={} fast_fired={} unchecked={} pending={pending} \
             lag_ms_p50={lag50} p90={lag90} p99={lag99} max={lagmax} gossip52_ms_p50={g50} \
             p90={g90} p99={g99} max={gmax} gossip52_reached={} gossip52_never={} votes={} \
             gossip_votes={} total_matched={} total_mismatched={} total_unchecked={} \
             poisoned={poisoned}",
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
        );
        solana_metrics::datapoint_info!(
            "fast_lane_cluster_check",
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

/// Replay froze `slot` with `hash` (also after a repair re-replay: a new version).
pub fn on_bank_frozen(slot: Slot, hash: Hash) {
    let now = Instant::now();
    let mut guard = state().lock();
    let st = &mut *guard;
    st.max_slot = st.max_slot.max(slot);
    {
        let rec = st.slots.entry(slot).or_default();
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
    for stats in [&mut st.interval, &mut st.total] {
        stats.frozen += 1;
    }
    st.check_fast(slot, now);
    st.prune();
    st.maybe_report(now);
}

/// The cluster confirmed `hash` for `slot`, and it equals our frozen hash.
pub fn on_cluster_match(slot: Slot, hash: Hash) {
    let now = Instant::now();
    let mut guard = state().lock();
    let st = &mut *guard;
    let lag_ms = st.lag_ms(slot, now);
    let recovered = match st.mismatched.iter().position(|m| *m == (slot, hash)) {
        Some(i) => {
            st.mismatched.remove(i);
            true
        }
        None => false,
    };
    let gossip52 = {
        let rec = st.slots.entry(slot).or_default();
        let first_verdict = !rec.verdict;
        rec.verdict = true;
        if rec.our_hash.is_none() {
            rec.our_hash = Some(hash);
        }
        first_verdict.then(|| (rec.frozen_at, rec.gossip_cross[1]))
    };
    for stats in [&mut st.interval, &mut st.total] {
        stats.matched += 1;
        if recovered {
            stats.recovered += 1;
        }
        if lag_ms.is_some_and(|lag| lag < 1) {
            stats.matched_at_freeze += 1;
        }
    }
    // Only the interval keeps samples.
    if let Some(lag) = lag_ms {
        st.interval.lag_ms.push(lag);
    }
    if let Some((Some(frozen_at), Some(crossed))) = gossip52 {
        st.interval
            .gossip52_ms
            .push(crossed.saturating_duration_since(frozen_at).as_millis() as u64);
    }
    if recovered {
        warn!(
            "fast lane: cluster check: slot {slot} replayed to the cluster's hash {hash} after a \
             mismatch (recovered)"
        );
    }
    let fl_was_active = control::is_active();
    st.push_event(Event {
        slot,
        kind: EventKind::Match { recovered },
        our_hash: Some(hash),
        cluster_hash: hash,
        lag_ms,
        fl_was_active,
    });
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
        "fast lane: CLUSTER BANK HASH MISMATCH: {reason}; fast lane was {}; poisoning it until \
         restart; agave dumps slot {slot} and its descendants and repairs and replays them",
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
    control::poison(&reason);
    let mut guard = state().lock();
    let st = &mut *guard;
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
