//! Program-derived-address derivation used by the runtime (the `sol_create_program_address` and
//! `sol_try_find_program_address` syscalls and CPI / native-invoke signer derivation).
//!
//! [`create_program_address`] returns exactly what [`Pubkey::create_program_address`] returns.
//! With `SOLANA_VM_PDA_CACHE=1` (see [`crate::vm_opts::PDA_CACHE`]) it keeps the seed checks and
//! the SHA-256, but memoizes the expensive ed25519 on-curve test (a ~250-squaring field
//! exponentiation, ~1.8 µs on FRA) in a bounded per-thread cache keyed by the full 32-byte
//! candidate hash. The on-curve test is a pure function of those 32 bytes, so a cached answer is
//! always the answer the test would give: a hit can only change the time taken, never the
//! result. Compute-unit charging happens in the callers and is untouched.

use {
    crate::vm_opts,
    solana_pubkey::{MAX_SEED_LEN, MAX_SEEDS, Pubkey, PubkeyError},
    std::{
        cell::RefCell,
        sync::atomic::{AtomicU64, Ordering},
    },
};

/// Must equal `solana_address::PDA_MARKER` (checked by the differential tests below).
const PDA_MARKER: &[u8; 21] = b"ProgramDerivedAddress";

/// Environment variable overriding the per-thread cache capacity (entries, rounded up to a
/// power of two and clamped to `[MIN_ENTRIES, MAX_ENTRIES]`).
pub const PDA_CACHE_ENTRIES_ENV: &str = "SOLANA_VM_PDA_CACHE_ENTRIES";
const DEFAULT_ENTRIES: usize = 8192;
const MIN_ENTRIES: usize = 256;
const MAX_ENTRIES: usize = 1 << 20;
const WAYS: usize = 4;
/// Per-thread lookups between flushes of the thread's counters into the global counters.
const STATS_FLUSH_INTERVAL: u64 = 4096;
/// Global lookups between hit-rate log lines.
const STATS_LOG_INTERVAL: u64 = 1 << 22;

/// Derives a program address. Identical to [`Pubkey::create_program_address`], including every
/// error; optionally faster (see the module docs).
#[inline]
pub fn create_program_address(seeds: &[&[u8]], program_id: &Pubkey) -> Result<Pubkey, PubkeyError> {
    if vm_opts::PDA_CACHE.enabled() {
        create_program_address_cached(seeds, program_id)
    } else {
        Pubkey::create_program_address(seeds, program_id)
    }
}

/// [`Pubkey::create_program_address`] (solana-address 2.7) with the on-curve test memoized.
fn create_program_address_cached(
    seeds: &[&[u8]],
    program_id: &Pubkey,
) -> Result<Pubkey, PubkeyError> {
    if seeds.len() > MAX_SEEDS {
        return Err(PubkeyError::MaxSeedLengthExceeded);
    }
    if seeds.iter().any(|seed| seed.len() > MAX_SEED_LEN) {
        return Err(PubkeyError::MaxSeedLengthExceeded);
    }
    let mut hasher = solana_sha256_hasher::Hasher::default();
    for seed in seeds.iter() {
        hasher.hash(seed);
    }
    hasher.hashv(&[program_id.as_ref(), PDA_MARKER]);
    let hash = hasher.result().to_bytes();
    if is_on_curve_cached(&hash) {
        return Err(PubkeyError::InvalidSeeds);
    }
    Ok(Pubkey::from(hash))
}

#[inline]
fn is_on_curve_uncached(bytes: &[u8; 32]) -> bool {
    solana_pubkey::bytes_are_curve_point(bytes)
}

/// Returns `bytes_are_curve_point(hash)`, from the calling thread's cache when possible.
fn is_on_curve_cached(hash: &[u8; 32]) -> bool {
    CURVE_CACHE
        .try_with(|cache| cache.borrow_mut().is_on_curve(hash))
        // Only during thread teardown.
        .unwrap_or_else(|_| is_on_curve_uncached(hash))
}

thread_local! {
    static CURVE_CACHE: RefCell<CurveCache> = RefCell::new(CurveCache::new(configured_entries()));
}

static GLOBAL_LOOKUPS: AtomicU64 = AtomicU64::new(0);
static GLOBAL_HITS: AtomicU64 = AtomicU64::new(0);

/// Process-wide `(lookups, hits)` of the on-curve cache, updated every
/// `STATS_FLUSH_INTERVAL` lookups per thread.
pub fn curve_cache_stats() -> (u64, u64) {
    (
        GLOBAL_LOOKUPS.load(Ordering::Relaxed),
        GLOBAL_HITS.load(Ordering::Relaxed),
    )
}

fn configured_entries() -> usize {
    std::env::var(PDA_CACHE_ENTRIES_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(clamp_entries)
        .unwrap_or(DEFAULT_ENTRIES)
}

fn clamp_entries(entries: usize) -> usize {
    entries
        .clamp(MIN_ENTRIES, MAX_ENTRIES)
        .next_power_of_two()
        .min(MAX_ENTRIES)
}

const EMPTY: u8 = 0;
const OFF_CURVE: u8 = 1;
const ON_CURVE: u8 = 2;

/// One 4-way set: keys, their cached answers and a FIFO replacement cursor.
#[derive(Clone)]
struct Set {
    keys: [[u8; 32]; WAYS],
    states: [u8; WAYS],
    next: u8,
}

impl Default for Set {
    fn default() -> Self {
        Self {
            keys: [[0; 32]; WAYS],
            states: [EMPTY; WAYS],
            next: 0,
        }
    }
}

/// Bounded, set-associative, exact cache of `bytes_are_curve_point`.
///
/// Correctness never depends on its contents: an entry is only returned for a lookup whose key
/// equals all 32 bytes of the stored key, and the stored answer was computed from those bytes.
/// Colliding or adversarially ground keys can only evict entries (a miss recomputes).
struct CurveCache {
    sets: Box<[Set]>,
    set_mask: usize,
    lookups: u64,
    hits: u64,
}

impl CurveCache {
    fn new(entries: usize) -> Self {
        let num_sets = clamp_entries(entries).checked_div(WAYS).unwrap().max(1);
        debug_assert!(num_sets.is_power_of_two());
        Self {
            sets: vec![Set::default(); num_sets].into_boxed_slice(),
            set_mask: num_sets.saturating_sub(1),
            lookups: 0,
            hits: 0,
        }
    }

    fn is_on_curve(&mut self, hash: &[u8; 32]) -> bool {
        self.lookups = self.lookups.wrapping_add(1);
        // SHA-256 output is uniformly distributed, so its low bits index the sets well.
        let index_bytes = hash.first_chunk::<8>().expect("32 > 8");
        let index = (u64::from_le_bytes(*index_bytes) as usize) & self.set_mask;
        let set = self
            .sets
            .get_mut(index)
            .expect("index is masked to the number of sets");
        for (key, state) in set.keys.iter().zip(set.states.iter()) {
            if *state != EMPTY && key == hash {
                self.hits = self.hits.wrapping_add(1);
                let on_curve = *state == ON_CURVE;
                self.maybe_flush_stats();
                return on_curve;
            }
        }
        let on_curve = is_on_curve_uncached(hash);
        let way = usize::from(set.next) % WAYS;
        if let (Some(key), Some(state)) = (set.keys.get_mut(way), set.states.get_mut(way)) {
            *key = *hash;
            *state = if on_curve { ON_CURVE } else { OFF_CURVE };
        }
        set.next = u8::try_from(way.wrapping_add(1) % WAYS).unwrap_or(0);
        self.maybe_flush_stats();
        on_curve
    }

    #[inline]
    fn maybe_flush_stats(&mut self) {
        if self.lookups < STATS_FLUSH_INTERVAL {
            return;
        }
        let lookups = std::mem::take(&mut self.lookups);
        let hits = std::mem::take(&mut self.hits);
        let before = GLOBAL_LOOKUPS.fetch_add(lookups, Ordering::Relaxed);
        let total_hits = GLOBAL_HITS
            .fetch_add(hits, Ordering::Relaxed)
            .wrapping_add(hits);
        let total = before.wrapping_add(lookups);
        if before / STATS_LOG_INTERVAL != total / STATS_LOG_INTERVAL {
            log::info!(
                "vm pda on-curve cache: {} lookups, {} hits ({:.1}%), {} entries per thread",
                total,
                total_hits,
                (total_hits as f64) * 100.0 / (total.max(1) as f64),
                self.sets.len().saturating_mul(WAYS),
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
mod tests {
    use {super::*, rand::Rng};

    fn random_bytes(rng: &mut impl Rng, len: usize) -> Vec<u8> {
        (0..len).map(|_| rng.random()).collect()
    }

    /// The cached derivation must agree with the upstream one on every input, including all
    /// error cases, whether the cache is cold or warm.
    #[test]
    fn test_cached_create_program_address_matches_upstream() {
        let mut rng = rand::rng();
        let mut cases: Vec<(Vec<Vec<u8>>, Pubkey)> = Vec::new();
        // Random seed shapes, including too many / too long seeds.
        for _ in 0..3000 {
            let num_seeds = rng.random_range(0..=MAX_SEEDS + 2);
            let seeds = (0..num_seeds)
                .map(|_| {
                    let len = if rng.random_ratio(1, 50) {
                        MAX_SEED_LEN + 1
                    } else {
                        rng.random_range(0..=MAX_SEED_LEN)
                    };
                    random_bytes(&mut rng, len)
                })
                .collect();
            cases.push((seeds, Pubkey::new_unique()));
        }
        // Full bump searches for a few fixed seeds: roughly half the bumps are on the curve.
        let program_id = Pubkey::new_unique();
        for seed in [&b"pool"[..], b"config", b"event_authority", b""] {
            for bump in 0..=u8::MAX {
                cases.push((vec![seed.to_vec(), vec![bump]], program_id));
            }
        }
        // Known upstream vectors.
        cases.push((vec![b"Talking".to_vec(), b"Squirrels".to_vec()], program_id));
        cases.push((vec![b"".to_vec(), vec![1]], program_id));
        cases.push((vec!["☉".as_bytes().to_vec(), vec![0]], program_id));

        let mut on_curve = 0usize;
        // Cold, then warm (every key cached), then warm again in reverse order.
        for pass in 0..3 {
            let order: Box<dyn Iterator<Item = &(Vec<Vec<u8>>, Pubkey)>> = if pass == 2 {
                Box::new(cases.iter().rev())
            } else {
                Box::new(cases.iter())
            };
            for (seeds, program_id) in order {
                let seeds: Vec<&[u8]> = seeds.iter().map(Vec::as_slice).collect();
                let expected = Pubkey::create_program_address(&seeds, program_id);
                let actual = create_program_address_cached(&seeds, program_id);
                assert_eq!(expected, actual, "seeds {seeds:?} program {program_id}");
                if pass == 0 && expected == Err(PubkeyError::InvalidSeeds) {
                    on_curve += 1;
                }
            }
        }
        // The corpus exercises both cached answers.
        assert!(on_curve > 100, "{on_curve}");
    }

    #[test]
    fn test_create_program_address_switch() {
        // Both settings of the switch give identical results (the switch is process-global,
        // and correct either way, so flipping it concurrently with other tests is harmless).
        let initial = vm_opts::PDA_CACHE.enabled();
        let program_id = Pubkey::new_unique();
        for bump in 0..=u8::MAX {
            let bump = [bump];
            let seeds: [&[u8]; 2] = [b"vault", &bump];
            let expected = Pubkey::create_program_address(&seeds, &program_id);
            vm_opts::PDA_CACHE.set(true);
            assert_eq!(create_program_address(&seeds, &program_id), expected);
            assert_eq!(create_program_address(&seeds, &program_id), expected);
            vm_opts::PDA_CACHE.set(false);
            assert_eq!(create_program_address(&seeds, &program_id), expected);
        }
        vm_opts::PDA_CACHE.set(initial);
    }

    #[test]
    fn test_curve_cache_hits_and_eviction() {
        let mut rng = rand::rng();
        let mut cache = CurveCache::new(MIN_ENTRIES);
        let keys: Vec<[u8; 32]> = (0..MIN_ENTRIES * 4).map(|_| rng.random()).collect();
        // A key's answer is always the uncached answer: first lookup (miss), second (hit).
        for key in keys.iter().take(8) {
            assert_eq!(cache.is_on_curve(key), is_on_curve_uncached(key));
            assert_eq!(cache.is_on_curve(key), is_on_curve_uncached(key));
        }
        assert_eq!(cache.hits, 8);
        // Overfill the cache (4x capacity); answers stay exact while entries are evicted.
        for _ in 0..2 {
            for key in &keys {
                assert_eq!(cache.is_on_curve(key), is_on_curve_uncached(key));
            }
        }
        // Keys that all map to set 0 (more keys than ways) only evict each other.
        let colliding: Vec<[u8; 32]> = (0..WAYS + 3)
            .map(|_| {
                let mut key: [u8; 32] = rng.random();
                key[..8].copy_from_slice(&0u64.to_le_bytes());
                key
            })
            .collect();
        for _ in 0..3 {
            for key in &colliding {
                assert_eq!(cache.is_on_curve(key), is_on_curve_uncached(key));
            }
        }
    }

    #[test]
    fn test_curve_cache_distinguishes_near_keys() {
        // Keys that share the index bytes and differ in one bit must not alias.
        let mut cache = CurveCache::new(MIN_ENTRIES);
        let mut rng = rand::rng();
        for _ in 0..500 {
            let key: [u8; 32] = rng.random();
            let mut near = key;
            near[31] ^= 1 << rng.random_range(0..8);
            assert_eq!(cache.is_on_curve(&key), is_on_curve_uncached(&key));
            assert_eq!(cache.is_on_curve(&near), is_on_curve_uncached(&near));
            assert_eq!(cache.is_on_curve(&key), is_on_curve_uncached(&key));
        }
    }

    #[test]
    fn test_clamp_entries() {
        assert_eq!(clamp_entries(0), MIN_ENTRIES);
        assert_eq!(clamp_entries(1000), 1024);
        assert_eq!(clamp_entries(8192), 8192);
        assert_eq!(clamp_entries(usize::MAX), MAX_ENTRIES);
    }

    #[test]
    fn test_pda_marker_matches_upstream() {
        // An address derived with our marker equals the upstream derivation for an off-curve
        // candidate, which pins the marker bytes.
        let program_id = Pubkey::new_unique();
        let (address, bump) = Pubkey::find_program_address(&[b"marker"], &program_id);
        assert_eq!(
            create_program_address_cached(&[b"marker", &[bump]], &program_id),
            Ok(address)
        );
    }

    /// Microbenchmark (run with `--release -- --ignored --nocapture`): a hot PDA re-derived with
    /// and without the cache.
    #[test]
    #[ignore]
    fn bench_create_program_address() {
        const ITERATIONS: u32 = 20_000;
        let program_id = Pubkey::new_unique();
        let (_, bump) = Pubkey::find_program_address(&[b"pool", program_id.as_ref()], &program_id);
        let bump = [bump];
        let seeds: [&[u8]; 3] = [b"pool", program_id.as_ref(), &bump];
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(Pubkey::create_program_address(
                std::hint::black_box(&seeds),
                &program_id,
            ))
            .unwrap();
        }
        let uncached = start.elapsed().as_nanos() / u128::from(ITERATIONS);
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(create_program_address_cached(
                std::hint::black_box(&seeds),
                &program_id,
            ))
            .unwrap();
        }
        let cached = start.elapsed().as_nanos() / u128::from(ITERATIONS);
        println!("create_program_address: uncached {uncached} ns, cached (hit) {cached} ns");
    }
}
