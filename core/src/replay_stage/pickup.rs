//! Opt-in changes that shorten *replay pickup*: the time between the blockstore gaining a
//! data-complete range and the replay thread handing its first transaction to the scheduler.
//!
//! Both are read once, when `ReplayStage` starts, and are off unless their environment variable
//! is set. With both unset the replay loop is the stock one.
//!
//! - [`REPLAY_FAST_PICKUP_ENV`]: on each wake-up, replay the already-active banks *before* looking
//!   for new banks to create (TowerBFT phase only), skip fork-map work the Alpenglow loop never
//!   uses, and remember a bank's passed chained-block-id check instead of re-reading two blockstore
//!   columns on every wake-up.
//! - [`REPLAY_STAGE_SPIN_US_ENV`]: busy-poll the replay loop's wake-up channels for a bounded time
//!   before parking, so a new range is picked up without a futex wake-up.

use {
    log::info,
    std::{
        sync::OnceLock,
        time::{Duration, Instant},
    },
};

/// Environment variable (`1`/`true`/`yes`/`on`) enabling the replay fast-pickup loop order.
///
/// Stock `ReplayStage` does, on every wake-up (roughly every insert that extends a slot's
/// contiguous shreds, ~150-450 times a second on mainnet): `generate_new_bank_forks` (a blockstore
/// `multi_get` of the `SlotMeta` of every frozen bank above the root, ~35 of them), builds the
/// ancestors and descendants maps of every bank, re-reads two blockstore columns to re-check the
/// active bank's chained block id, and only then replays the new entries. Mid-slot none of this
/// can change the outcome: a child bank is only ever created for a *frozen* parent, so while the
/// current slot is being replayed there is nothing new to create.
///
/// With the flag on:
/// - While TowerBFT is in charge (the Alpenglow feature is not yet activated), the loop replays the
///   active banks first, then runs `generate_new_bank_forks` for the parents that were frozen
///   *before* this iteration (exactly the set the stock order would use), replays any bank it just
///   created in the same iteration, and only then builds the fork maps for the TowerBFT control
///   path. The set of banks, the maps the control path sees and the order of slot notifications
///   (frozen, then rooted, then the child's creation) are the same as in the stock order; only the
///   first replay of an iteration no longer waits for bank discovery. From the moment the
///   Alpenglow feature activates the loop reverts to the stock order.
/// - Once Alpenglow is enabled, the ancestors/descendants maps are no longer built before replay:
///   the Alpenglow loop only needs them to enable Alpenglow, and builds them then.
/// - A bank whose chained block id check passed is not re-checked: its shred 0 and its parent's
///   last shred cannot change without the slot (and with it the bank and its progress entry)
///   being purged and recreated.
pub const REPLAY_FAST_PICKUP_ENV: &str = "SOLANA_REPLAY_FAST_PICKUP";

/// Environment variable bounding how long (in microseconds) the replay thread busy-polls its
/// wake-up channels (blockstore signal, bank-forks commands, set-root signal) before it parks in
/// its `select!`. Unset or `0` keeps the stock behavior; values are capped at
/// [`MAX_REPLAY_STAGE_SPIN`].
///
/// A parked replay thread first has to be woken (futex, possibly an idle-state exit and a
/// migration to another CPU) before it can start on a range; on a busy CPU set that is tens to
/// hundreds of microseconds. Spinning keeps it on its CPU with warm caches. Only timing and CPU
/// usage change: the `select!` that follows is unchanged, so the loop takes the same branch on the
/// same message. Meant for a replay thread pinned to a dedicated core; mainnet inserts wake replay
/// every ~2-7 ms, so a budget of a few milliseconds keeps it awake between them.
pub const REPLAY_STAGE_SPIN_US_ENV: &str = "SOLANA_REPLAY_STAGE_SPIN_US";
/// Upper bound for [`REPLAY_STAGE_SPIN_US_ENV`]: the replay loop's own idle timeout.
pub const MAX_REPLAY_STAGE_SPIN: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReplayPickupOptions {
    /// See [`REPLAY_FAST_PICKUP_ENV`].
    pub(crate) fast_pickup: bool,
    /// See [`REPLAY_STAGE_SPIN_US_ENV`].
    pub(crate) spin_before_wait: Option<Duration>,
}

impl ReplayPickupOptions {
    pub(crate) fn from_env() -> Self {
        static OPTIONS: OnceLock<ReplayPickupOptions> = OnceLock::new();
        *OPTIONS.get_or_init(|| {
            let options = Self::parse(
                std::env::var(REPLAY_FAST_PICKUP_ENV).ok().as_deref(),
                std::env::var(REPLAY_STAGE_SPIN_US_ENV).ok().as_deref(),
            );
            info!(
                "replay pickup: fast pickup {} ({REPLAY_FAST_PICKUP_ENV}), spin before wait {:?} \
                 ({REPLAY_STAGE_SPIN_US_ENV})",
                if options.fast_pickup { "on" } else { "off" },
                options.spin_before_wait,
            );
            options
        })
    }

    fn parse(fast_pickup: Option<&str>, spin_us: Option<&str>) -> Self {
        Self {
            fast_pickup: fast_pickup.is_some_and(parse_bool_flag),
            spin_before_wait: spin_us.and_then(parse_spin),
        }
    }
}

fn parse_bool_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn parse_spin(value: &str) -> Option<Duration> {
    let micros = value.trim().parse::<u64>().ok()?;
    (micros > 0).then(|| Duration::from_micros(micros).min(MAX_REPLAY_STAGE_SPIN))
}

/// Busy-polls `is_ready` for at most `budget` and returns as soon as it holds.
///
/// The caller follows this with its normal blocking `select!`, which then finds the ready channel
/// without parking. `is_ready` must be side-effect free (e.g. `Receiver::is_empty`).
#[inline]
pub(crate) fn spin_until_ready(budget: Duration, mut is_ready: impl FnMut() -> bool) {
    // Checks between clock reads; keeps `Instant::now()` off the per-iteration path.
    const POLLS_PER_CLOCK_READ: usize = 64;

    if is_ready() {
        return;
    }
    let started = Instant::now();
    loop {
        for _ in 0..POLLS_PER_CLOCK_READ {
            std::hint::spin_loop();
            if is_ready() {
                return;
            }
        }
        if started.elapsed() >= budget {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    #[test]
    fn test_parse_replay_pickup_options() {
        assert_eq!(
            ReplayPickupOptions::parse(None, None),
            ReplayPickupOptions::default()
        );
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(ReplayPickupOptions::parse(Some(value), None).fast_pickup);
        }
        for value in ["", "0", "false", "off", "no", "2"] {
            assert!(!ReplayPickupOptions::parse(Some(value), None).fast_pickup);
        }
        assert_eq!(
            ReplayPickupOptions::parse(None, Some("3000")).spin_before_wait,
            Some(Duration::from_millis(3))
        );
        assert_eq!(
            ReplayPickupOptions::parse(None, Some("100000000")).spin_before_wait,
            Some(MAX_REPLAY_STAGE_SPIN)
        );
        for value in ["", "0", "-5", "fast"] {
            assert_eq!(
                ReplayPickupOptions::parse(None, Some(value)).spin_before_wait,
                None,
                "{value}"
            );
        }
    }

    #[test]
    fn test_spin_until_ready_returns_when_ready() {
        let polls = AtomicUsize::new(0);
        spin_until_ready(Duration::from_secs(60), || {
            polls.fetch_add(1, Ordering::Relaxed) >= 100
        });
        assert!(polls.load(Ordering::Relaxed) > 100);

        let ready = Arc::new(AtomicBool::new(false));
        let setter = {
            let ready = ready.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(5));
                ready.store(true, Ordering::Release);
            })
        };
        let started = Instant::now();
        spin_until_ready(Duration::from_secs(60), || ready.load(Ordering::Acquire));
        assert!(ready.load(Ordering::Acquire));
        assert!(started.elapsed() < Duration::from_secs(30));
        setter.join().unwrap();
    }

    #[test]
    fn test_spin_until_ready_respects_budget() {
        let started = Instant::now();
        spin_until_ready(Duration::from_millis(2), || false);
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(2));
        assert!(elapsed < Duration::from_secs(5));
    }
}
