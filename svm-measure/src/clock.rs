//! Monotonic timestamps for timing metrics, optionally read from the CPU's cycle counter.
//!
//! By default [`Timestamp`] wraps [`std::time::Instant`], i.e. `clock_gettime(CLOCK_MONOTONIC)`.
//! On Linux x86_64 that vDSO call reads the TSC with `rdtscp` (or `lfence; rdtsc`), which waits
//! for all older instructions to retire, plus a seqlock and scaling: ~40-120 ns per read on FRA,
//! and runtime execution takes ~12 reads per SBF invocation and a few per loaded account, all
//! only to feed timing metrics.
//!
//! With `SOLANA_VM_CHEAP_TIMERS=1` (read once per process, default off) timestamps come from a
//! plain, non-serializing counter read instead: `rdtsc` on x86_64 (only when the CPU reports an
//! invariant TSC and, on Linux, the kernel's clocksource is `tsc`, i.e. the kernel itself trusts
//! the TSC to be synchronized across CPUs) or `cntvct_el0` on aarch64. Ticks are converted to
//! nanoseconds with a factor calibrated once against `Instant` (x86_64) or read from
//! `cntfrq_el0` (aarch64). If the counter is unsuitable the switch falls back to `Instant`.
//!
//! Durations keep their meaning (nanoseconds of wall time, accurate to the calibration, ~1e-4)
//! but a single measurement may be skewed by the few instructions a non-serializing read can
//! be reordered across, which is irrelevant for metrics. Timestamps are only ever used for
//! metrics and cache-recency bookkeeping, never for results.
//!
//! Each [`Timestamp`] remembers which clock produced it, so flipping the switch (tests only)
//! between taking and reading a timestamp is harmless.

use std::{
    fmt,
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

/// Environment variable enabling the cycle-counter clock.
pub const CHEAP_TIMERS_ENV: &str = "SOLANA_VM_CHEAP_TIMERS";

const UNINITIALIZED: u8 = 0;
const DISABLED: u8 = 1;
const ENABLED: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(UNINITIALIZED);
static TICK_CLOCK: OnceLock<Option<TickClock>> = OnceLock::new();

/// A point in time for measuring elapsed durations.
#[derive(Clone, Copy)]
pub struct Timestamp(Repr);

#[derive(Clone, Copy)]
enum Repr {
    Instant(Instant),
    Ticks {
        ticks: u64,
        clock: &'static TickClock,
    },
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Repr::Instant(instant) => f.debug_tuple("Timestamp").field(instant).finish(),
            Repr::Ticks { ticks, .. } => f.debug_struct("Timestamp").field("ticks", ticks).finish(),
        }
    }
}

impl Timestamp {
    /// The current time.
    #[inline]
    pub fn now() -> Self {
        match tick_clock() {
            Some(clock) => Timestamp(Repr::Ticks {
                ticks: clock.read(),
                clock,
            }),
            None => Timestamp(Repr::Instant(Instant::now())),
        }
    }

    /// Time elapsed since `self` (zero if the counter appears to have gone backwards).
    #[inline]
    pub fn elapsed(&self) -> Duration {
        match self.0 {
            Repr::Instant(instant) => instant.elapsed(),
            Repr::Ticks { ticks, clock } => {
                Duration::from_nanos(clock.ticks_to_ns(clock.read().saturating_sub(ticks)))
            }
        }
    }

    /// Whether this timestamp came from the cycle-counter clock.
    pub fn is_cheap(&self) -> bool {
        matches!(self.0, Repr::Ticks { .. })
    }
}

/// Whether `SOLANA_VM_CHEAP_TIMERS` is on (it may still fall back to `Instant` if the counter is
/// unsuitable; see [`cheap_clock_available`]).
#[inline]
pub fn cheap_timers_enabled() -> bool {
    match MODE.load(Ordering::Relaxed) {
        ENABLED => true,
        DISABLED => false,
        _ => init_mode_from_env(),
    }
}

/// Forces the switch on or off. For tests and benchmarks only.
#[doc(hidden)]
pub fn set_cheap_timers(enabled: bool) {
    MODE.store(if enabled { ENABLED } else { DISABLED }, Ordering::Relaxed);
}

/// Whether this machine has a usable cycle-counter clock (calibrating it on first call).
pub fn cheap_clock_available() -> bool {
    TICK_CLOCK.get_or_init(TickClock::new).is_some()
}

#[cold]
fn init_mode_from_env() -> bool {
    let enabled = std::env::var(CHEAP_TIMERS_ENV)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    let _ = MODE.compare_exchange(
        UNINITIALIZED,
        if enabled { ENABLED } else { DISABLED },
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    MODE.load(Ordering::Relaxed) == ENABLED
}

#[inline]
fn tick_clock() -> Option<&'static TickClock> {
    if !cheap_timers_enabled() {
        return None;
    }
    TICK_CLOCK.get_or_init(TickClock::new).as_ref()
}

/// A free-running, invariant hardware counter and its tick-to-nanosecond factor.
#[derive(Debug)]
pub struct TickClock {
    /// Nanoseconds per tick as a 32.32 fixed-point number.
    ns_per_tick_q32: u64,
}

impl TickClock {
    fn new() -> Option<Self> {
        let clock = Self::detect();
        match &clock {
            Some(clock) => eprintln!(
                "{CHEAP_TIMERS_ENV}: timing metrics use the cycle counter ({:.4} ns per tick)",
                clock.ns_per_tick()
            ),
            None => eprintln!(
                "{CHEAP_TIMERS_ENV}: no invariant cycle counter usable as a clock; using Instant"
            ),
        }
        clock
    }

    #[cfg(target_arch = "x86_64")]
    fn detect() -> Option<Self> {
        if !x86_64_invariant_tsc() || !linux_clocksource_is_tsc() {
            return None;
        }
        Self::calibrate_against_instant(Duration::from_millis(2))
    }

    #[cfg(target_arch = "aarch64")]
    fn detect() -> Option<Self> {
        let frequency = aarch64_counter_frequency();
        if frequency == 0 {
            return None;
        }
        let ns_per_tick_q32 = (1_000_000_000u128 << 32).checked_div(u128::from(frequency))?;
        Some(Self {
            ns_per_tick_q32: u64::try_from(ns_per_tick_q32).ok()?,
        })
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn detect() -> Option<Self> {
        None
    }

    #[cfg(target_arch = "x86_64")]
    fn calibrate_against_instant(window: Duration) -> Option<Self> {
        let start_instant = Instant::now();
        let start_ticks = read_counter();
        while start_instant.elapsed() < window {
            std::hint::spin_loop();
        }
        let end_ticks = read_counter();
        let elapsed_ns = start_instant.elapsed().as_nanos();
        let ticks = end_ticks
            .checked_sub(start_ticks)
            .filter(|ticks| *ticks > 0)?;
        let ns_per_tick_q32 = (elapsed_ns << 32).checked_div(u128::from(ticks))?;
        Some(Self {
            ns_per_tick_q32: u64::try_from(ns_per_tick_q32).ok()?,
        })
    }

    #[inline]
    fn read(&self) -> u64 {
        read_counter()
    }

    #[inline]
    fn ticks_to_ns(&self, ticks: u64) -> u64 {
        let ns = (u128::from(ticks) * u128::from(self.ns_per_tick_q32)) >> 32;
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    /// Nanoseconds per tick (for diagnostics).
    pub fn ns_per_tick(&self) -> f64 {
        self.ns_per_tick_q32 as f64 / (1u64 << 32) as f64
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn read_counter() -> u64 {
    // SAFETY: `rdtsc` is available on every x86_64 CPU and has no preconditions.
    #[allow(unused_unsafe)]
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn read_counter() -> u64 {
    let ticks: u64;
    // SAFETY: CNTVCT_EL0 is readable from EL0 on Linux and macOS.
    unsafe {
        core::arch::asm!("mrs {}, cntvct_el0", out(reg) ticks, options(nomem, nostack));
    }
    ticks
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[inline]
fn read_counter() -> u64 {
    0
}

#[cfg(target_arch = "aarch64")]
fn aarch64_counter_frequency() -> u64 {
    let frequency: u64;
    // SAFETY: CNTFRQ_EL0 is readable from EL0 on Linux and macOS.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) frequency, options(nomem, nostack));
    }
    frequency
}

#[cfg(target_arch = "x86_64")]
fn x86_64_invariant_tsc() -> bool {
    use core::arch::x86_64::__cpuid;
    // SAFETY: `cpuid` is available on every x86_64 CPU.
    #[allow(unused_unsafe)]
    let max_extended_leaf = unsafe { __cpuid(0x8000_0000) }.eax;
    if max_extended_leaf < 0x8000_0007 {
        return false;
    }
    // CPUID.80000007H:EDX[8] = invariant TSC (constant rate across P-, C- and T-states).
    #[allow(unused_unsafe)]
    let advanced_power_management = unsafe { __cpuid(0x8000_0007) };
    advanced_power_management.edx & (1 << 8) != 0
}

#[cfg(target_arch = "x86_64")]
fn linux_clocksource_is_tsc() -> bool {
    if cfg!(target_os = "linux") {
        std::fs::read_to_string("/sys/devices/system/clocksource/clocksource0/current_clocksource")
            .map(|clocksource| clocksource.trim() == "tsc")
            .unwrap_or(false)
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::thread::sleep};

    #[test]
    fn test_cheap_clock_tracks_instant() {
        if !cheap_clock_available() {
            return;
        }
        set_cheap_timers(true);
        let cheap = Timestamp::now();
        let instant = Instant::now();
        assert!(cheap.is_cheap());
        sleep(Duration::from_millis(50));
        let cheap_elapsed = cheap.elapsed().as_nanos() as f64;
        let instant_elapsed = instant.elapsed().as_nanos() as f64;
        // Generous bound: the counter frequency on some machines is only ~24 MHz.
        let ratio = cheap_elapsed / instant_elapsed;
        assert!((0.97..1.03).contains(&ratio), "ratio {ratio}");
        set_cheap_timers(false);
    }

    #[test]
    fn test_timestamp_keeps_its_clock() {
        set_cheap_timers(false);
        let stock = Timestamp::now();
        assert!(!stock.is_cheap());
        if cheap_clock_available() {
            set_cheap_timers(true);
            let cheap = Timestamp::now();
            assert!(cheap.is_cheap());
            // Flipping the switch does not change how an existing timestamp is read.
            set_cheap_timers(false);
            sleep(Duration::from_millis(2));
            assert!(cheap.elapsed() >= Duration::from_millis(1));
        }
        assert!(stock.elapsed() >= Duration::from_millis(1) || !cheap_clock_available());
        set_cheap_timers(false);
    }

    #[test]
    fn test_ticks_to_ns() {
        let clock = TickClock {
            // 0.25 ns per tick (4 GHz).
            ns_per_tick_q32: 1 << 30,
        };
        assert_eq!(clock.ticks_to_ns(0), 0);
        assert_eq!(clock.ticks_to_ns(4_000_000_000), 1_000_000_000);
        assert_eq!(clock.ticks_to_ns(u64::MAX), u64::MAX / 4);
        assert!((clock.ns_per_tick() - 0.25).abs() < 1e-12);
    }

    /// Microbenchmark (run with `--release -- --ignored --nocapture`): cost of taking and reading
    /// a timestamp with each clock.
    #[test]
    #[ignore]
    fn bench_timestamp() {
        const ITERATIONS: u32 = 1_000_000;
        for cheap in [false, true] {
            set_cheap_timers(cheap);
            let start = Instant::now();
            let mut sum = 0u128;
            for _ in 0..ITERATIONS {
                let timestamp = std::hint::black_box(Timestamp::now());
                sum += timestamp.elapsed().as_nanos();
            }
            std::hint::black_box(sum);
            let ns = start.elapsed().as_nanos() / u128::from(ITERATIONS);
            println!(
                "Timestamp::now()+elapsed() with cheap timers {}: {ns} ns (2 clock reads)",
                if cheap { "on" } else { "off" }
            );
        }
        set_cheap_timers(false);
    }
}
