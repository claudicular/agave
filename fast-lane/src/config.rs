//! Fast-lane configuration.
//!
//! Read once at startup from the file named by `AGAVE_FAST_LANE_CONFIG`. The format is a
//! small TOML subset: one `key = value` per line, `#` comments, values are integers,
//! floats, booleans, quoted or bare strings, or `[a, b, c]` integer lists. Unknown keys are
//! rejected so typos are caught at boot.

use {
    solana_pubkey::Pubkey,
    std::{fmt, path::PathBuf, str::FromStr},
};

pub const CONFIG_ENV: &str = "AGAVE_FAST_LANE_CONFIG";

#[derive(Debug, Clone)]
pub struct Config {
    /// Install the taps and tees and start the fast lane at boot.
    pub enabled: bool,
    /// Number of executor threads (K).
    pub workers: usize,
    /// Cores the executor threads are pinned to, round-robin. Empty = unpinned (niced).
    /// There is deliberately no default: the operator assigns cores at deploy time (FRA's
    /// replay handlers own 15-22 and a sampler owns 23; see DESIGN §3.3).
    pub worker_cores: Vec<usize>,
    /// Core for the coordinator thread. None = unpinned (niced).
    pub sched_core: Option<usize>,
    /// Core for the ingest thread. None = unpinned (niced).
    pub ingest_core: Option<usize>,
    /// CPU set applied to every FL thread that has no dedicated core (coordinator/ingest when
    /// unpinned, comparator, control, and workers when `worker_cores` is empty). Empty =
    /// inherit the spawning thread's mask. Accepts ranges: `[0-11, 24-35]`.
    pub shared_cores: Vec<usize>,
    /// Nice value for unpinned latency threads (coordinator, ingest, workers).
    pub nice: i32,
    /// Nice value for auxiliary threads (comparator, control).
    pub aux_nice: i32,
    /// Busy-poll time before parking, in microseconds, for the coordinator.
    /// Values >= 1_000_000 spin forever (never park): use only on a dedicated core.
    pub spin_us: u64,
    /// Busy-poll time before parking for executor threads (raise when they own their cores).
    /// Values >= 1_000_000 spin forever (never park).
    pub worker_spin_us: u64,
    /// Busy-poll time before parking for the ingest thread (0 = block). >= 1_000_000: forever.
    pub ingest_spin_us: u64,
    /// Optimistic (speculative) execution. false = pure MVCC (dispatch only when every
    /// predecessor is final).
    pub speculation: bool,
    /// Speculation threshold on `pending_writers * change_probability`.
    pub theta: f32,
    /// EWMA weight of the per-account change-probability hint.
    pub hint_alpha: f32,
    /// Maximum speculative incarnations per transaction before it waits for its
    /// predecessors.
    pub max_incarnations: u32,
    /// Re-dispatch executed/running readers as soon as a stale read is detected.
    pub eager_reexec: bool,
    /// Delta-rebase value prediction (needs `eager_reexec`): when a speculatively executed
    /// transaction's input changes, install a predicted output (its own change re-applied to
    /// the new input) for later transactions to speculate on, and let accounts whose
    /// predictions verify (fee payers, fee sinks) be speculated past. Exactness is unchanged:
    /// predictions are never final and every FINAL read is checked against executed values.
    /// Runtime toggle: `rebase=on|off` in the control file.
    pub rebase: bool,
    /// Result-identical VM shortcuts on FL's executor threads only (`control::vm_opts`):
    /// mapped-prefix heap reset, PDA on-curve cache, pooled program-input buffers — agave's
    /// `SOLANA_VM_HEAP_ZERO_OPT`, `SOLANA_VM_PDA_CACHE`, `SOLANA_VM_SER_POOL`, without
    /// setting them for agave's threads. Runtime toggle: `vm_opts=on|off` in the control file.
    pub vm_opts: bool,
    /// Give up on a slot whose parent does not freeze within this time.
    pub parent_wait_ms: u64,
    /// Maximum concurrently active runs.
    pub max_runs: usize,
    /// Hard cap on the memory the fast lane holds (overlays, comparator frames and records,
    /// queues, pending entries, hints; see `mem`). Above it FL disables itself and releases
    /// everything; no new run starts while overlays hold more than half of it.
    pub mem_cap_mb: usize,
    /// Run the shadow comparator against agave's grouped notifications.
    pub comparator: bool,
    /// Directory for the per-transaction export and mismatch samples.
    /// Default: `<ledger>/fast_lane`.
    pub export_dir: Option<PathBuf>,
    /// Size of one export file before rotating, in MB.
    pub export_file_mb: u64,
    /// Number of export files kept (oldest deleted).
    pub export_files: usize,
    /// Export vote transactions too.
    pub export_votes: bool,
    /// Mismatch samples written per minute (full account diffs).
    pub mismatch_samples_per_min: u32,
    /// Summary log interval.
    pub summary_interval_s: u64,
    /// Control file polled every 250 ms. Default: `<ledger>/fast_lane.ctl`.
    pub control_file: Option<PathBuf>,
    /// Allow the `panic` control command (panic-containment drills).
    pub allow_panic_command: bool,
    /// Input: `blockstore` (agave's completed data sets) or `dual` (also the proxy ring v2).
    pub input_dual: bool,
    /// Path of the proxy's ring v2 (`SHMEM_RING_V2_PATH` of the proxy).
    pub ring_path: Option<PathBuf>,
    /// With dual input, also read and cross-check blockstore batches the ring already
    /// delivered (costs a blockstore read per batch on the ingest thread).
    pub ring_blockstore_check: bool,
    /// Start a child's run on top of FL's own (complete) run of its parent when agave has
    /// not frozen the parent yet (phase 2b). The parent's freeze-time writes and the child's
    /// SlotHashes entry become known when agave freezes the parent; transactions that read
    /// them re-execute then.
    pub chain: bool,
    /// Phase 3: publish FINAL transactions' account updates into a shared-memory ring.
    pub out_ring: bool,
    pub out_ring_path: PathBuf,
    /// Data region size of the output ring.
    pub out_ring_mb: usize,
    /// Include accounts owned by SPL Token and Token-2022.
    pub out_token: bool,
    /// Further owners (programs) whose accounts are published.
    pub out_owners: Vec<Pubkey>,
    /// Commit mode (`crate::control::COMMIT_*`): `off`; `shadow` (milestone 1: FL keeps
    /// every transaction's full processing result, executed with agave's recording
    /// configuration, and the comparator checks it field by field against agave's own); or
    /// `on` (milestone 2: FL commits its results into agave's banks, agave follows).
    /// Runtime toggle: `commit=off|shadow|on` in the control file.
    pub commit: u8,
    /// Commit threads (`solFlCommitNN`), committing FINAL transactions into agave's banks.
    pub commit_threads: usize,
    /// Cores the commit threads are pinned to, round-robin. Empty = unpinned (niced, on
    /// `shared_cores`).
    pub commit_cores: Vec<usize>,
    /// Busy-poll before parking for commit threads (>= 1_000_000: never park).
    pub commit_spin_us: u64,
    /// Commit mode: parts per million of transactions agave executes itself, at their
    /// position, and compares with FL's result before committing its own; any difference
    /// poisons FL (and replays the slot without FL if FL committed into it). Runtime toggle:
    /// `verify_sample_ppm=N` (banks created afterwards).
    pub verify_sample_ppm: u32,
    /// Commit mode: how long agave's replay waits for FL to claim a transaction before
    /// executing it itself. Runtime toggle: `follow_wait_ms=N`.
    pub follow_wait_ms: u64,
    /// Commit mode: a replay handler waiting for FL spins this long, then sleeps 20 µs
    /// between checks (low values free the handlers' cores).
    pub follow_spin_us: u64,
    /// Commit on FL's executor threads (`commit_on = workers`) rather than on the commit
    /// threads (`threads`). Runtime toggle: `commit_on=workers|threads`.
    pub commit_on_workers: bool,
    /// How the verification sample is chosen: `fl` (FL picks transactions no later
    /// transaction conflicts with; samples never delay other commits) or `hash`
    /// (deterministic on the signature). Runtime toggle `sample_mode=fl|hash` (new banks).
    pub sample_mode: u8,
    /// Commit mode: how long a replay handler waits for FL's binding of a bank FL runs (µs).
    /// Runtime toggle `bind_wait_us=N`.
    pub bind_wait_us: u64,
    /// Parts per million of committed transactions written to `fl_commit.*.csv`. Runtime
    /// toggle `commit_csv_ppm=N`.
    pub commit_csv_ppm: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            workers: 2,
            worker_cores: Vec::new(),
            sched_core: None,
            ingest_core: None,
            shared_cores: Vec::new(),
            nice: 5,
            aux_nice: 10,
            spin_us: 50,
            worker_spin_us: 50,
            ingest_spin_us: 0,
            speculation: true,
            theta: 0.2,
            hint_alpha: 1.0 / 32.0,
            max_incarnations: 3,
            eager_reexec: true,
            rebase: false,
            vm_opts: false,
            parent_wait_ms: 300,
            max_runs: 4,
            mem_cap_mb: 1024,
            comparator: true,
            export_dir: None,
            export_file_mb: 256,
            export_files: 8,
            export_votes: false,
            mismatch_samples_per_min: 20,
            summary_interval_s: 10,
            control_file: None,
            allow_panic_command: false,
            input_dual: false,
            ring_path: None,
            ring_blockstore_check: false,
            chain: false,
            out_ring: false,
            out_ring_path: PathBuf::from("/dev/shm/fastlane.out.ring"),
            out_ring_mb: 256,
            out_token: true,
            out_owners: Vec::new(),
            commit: crate::control::COMMIT_OFF,
            commit_threads: 2,
            commit_cores: Vec::new(),
            commit_spin_us: 50,
            verify_sample_ppm: 0,
            follow_wait_ms: 100,
            follow_spin_us: 200,
            commit_on_workers: false,
            sample_mode: solana_runtime::fast_lane_commit::SAMPLE_FL,
            bind_wait_us: 2_000,
            commit_csv_ppm: 100_000,
        }
    }
}

#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fast lane config: {}", self.0)
    }
}

fn parse_scalar<T: FromStr>(key: &str, value: &str) -> Result<T, ConfigError> {
    value
        .parse::<T>()
        .map_err(|_| ConfigError(format!("bad value for {key}: {value}")))
}

fn parse_bool(key: &str, value: &str) -> Result<bool, ConfigError> {
    match value {
        "true" | "1" | "on" | "yes" => Ok(true),
        "false" | "0" | "off" | "no" => Ok(false),
        _ => Err(ConfigError(format!("bad bool for {key}: {value}"))),
    }
}

fn parse_list(key: &str, value: &str) -> Result<Vec<usize>, ConfigError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .ok_or_else(|| ConfigError(format!("{key} must be a [list]")))?;
    let mut out = Vec::new();
    for item in inner.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match item.split_once('-') {
            Some((lo, hi)) => {
                let lo = parse_scalar::<usize>(key, lo.trim())?;
                let hi = parse_scalar::<usize>(key, hi.trim())?;
                if lo > hi || hi >= 1024 {
                    return Err(ConfigError(format!("bad range in {key}: {item}")));
                }
                out.extend(lo..=hi);
            }
            None => out.push(parse_scalar::<usize>(key, item)?),
        }
    }
    Ok(out)
}

fn parse_pubkeys(key: &str, value: &str) -> Result<Vec<Pubkey>, ConfigError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .ok_or_else(|| ConfigError(format!("{key} must be a [list]")))?;
    inner
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|item| {
            Pubkey::from_str(unquote(item))
                .map_err(|_| ConfigError(format!("bad pubkey in {key}: {item}")))
        })
        .collect()
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut config = Config::default();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with('[') && !line.contains('=') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| ConfigError(format!("line {}: expected key = value", lineno + 1)))?;
            let key = key.trim();
            let value = unquote(value.trim());
            config.set(key, value)?;
        }
        config.validate()?;
        Ok(config)
    }

    /// Set one key (also used by the control file for runtime-tunable keys).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        match key {
            "enabled" => self.enabled = parse_bool(key, value)?,
            "workers" => self.workers = parse_scalar(key, value)?,
            "worker_cores" => self.worker_cores = parse_list(key, value)?,
            "sched_core" => {
                self.sched_core = if value == "none" {
                    None
                } else {
                    Some(parse_scalar(key, value)?)
                }
            }
            "ingest_core" => {
                self.ingest_core = if value == "none" {
                    None
                } else {
                    Some(parse_scalar(key, value)?)
                }
            }
            "shared_cores" => self.shared_cores = parse_list(key, value)?,
            "nice" => self.nice = parse_scalar(key, value)?,
            "aux_nice" => self.aux_nice = parse_scalar(key, value)?,
            "spin_us" => self.spin_us = parse_scalar(key, value)?,
            "worker_spin_us" => self.worker_spin_us = parse_scalar(key, value)?,
            "ingest_spin_us" => self.ingest_spin_us = parse_scalar(key, value)?,
            "speculation" => self.speculation = parse_bool(key, value)?,
            "theta" => self.theta = parse_scalar(key, value)?,
            "hint_alpha" => self.hint_alpha = parse_scalar(key, value)?,
            "max_incarnations" => self.max_incarnations = parse_scalar(key, value)?,
            "eager_reexec" => self.eager_reexec = parse_bool(key, value)?,
            "rebase" => self.rebase = parse_bool(key, value)?,
            "vm_opts" => self.vm_opts = parse_bool(key, value)?,
            "parent_wait_ms" => self.parent_wait_ms = parse_scalar(key, value)?,
            "max_runs" => self.max_runs = parse_scalar(key, value)?,
            "mem_cap_mb" => self.mem_cap_mb = parse_scalar(key, value)?,
            "comparator" => self.comparator = parse_bool(key, value)?,
            "export_dir" => self.export_dir = Some(PathBuf::from(value)),
            "export_file_mb" => self.export_file_mb = parse_scalar(key, value)?,
            "export_files" => self.export_files = parse_scalar(key, value)?,
            "export_votes" => self.export_votes = parse_bool(key, value)?,
            "mismatch_samples_per_min" => self.mismatch_samples_per_min = parse_scalar(key, value)?,
            "summary_interval_s" => self.summary_interval_s = parse_scalar(key, value)?,
            "control_file" => self.control_file = Some(PathBuf::from(value)),
            "allow_panic_command" => self.allow_panic_command = parse_bool(key, value)?,
            "mode" => {
                if value != "shadow" {
                    return Err(ConfigError(format!("mode={value} not supported")));
                }
            }
            "input" => {
                self.input_dual = match value {
                    "blockstore" => false,
                    "dual" => true,
                    _ => return Err(ConfigError(format!("input={value} not supported"))),
                }
            }
            "ring_path" => self.ring_path = Some(PathBuf::from(value)),
            "ring_blockstore_check" => self.ring_blockstore_check = parse_bool(key, value)?,
            "chain" => self.chain = parse_bool(key, value)?,
            "out_ring" => self.out_ring = parse_bool(key, value)?,
            "out_ring_path" => self.out_ring_path = PathBuf::from(value),
            "out_ring_mb" => self.out_ring_mb = parse_scalar(key, value)?,
            "out_token" => self.out_token = parse_bool(key, value)?,
            "out_owners" => self.out_owners = parse_pubkeys(key, value)?,
            "commit" => {
                self.commit = crate::control::parse_commit_mode(value)
                    .ok_or_else(|| ConfigError(format!("bad commit mode {value}")))?
            }
            "commit_threads" => self.commit_threads = parse_scalar(key, value)?,
            "commit_cores" => self.commit_cores = parse_list(key, value)?,
            "commit_spin_us" => self.commit_spin_us = parse_scalar(key, value)?,
            "verify_sample_ppm" => self.verify_sample_ppm = parse_scalar(key, value)?,
            "follow_wait_ms" => self.follow_wait_ms = parse_scalar(key, value)?,
            "follow_spin_us" => self.follow_spin_us = parse_scalar(key, value)?,
            "commit_on" => {
                self.commit_on_workers = match value {
                    "workers" => true,
                    "threads" => false,
                    _ => return Err(ConfigError(format!("bad commit_on {value}"))),
                }
            }
            "sample_mode" => {
                self.sample_mode = match value {
                    "fl" => solana_runtime::fast_lane_commit::SAMPLE_FL,
                    "hash" => solana_runtime::fast_lane_commit::SAMPLE_HASH,
                    _ => return Err(ConfigError(format!("bad sample_mode {value}"))),
                }
            }
            "bind_wait_us" => self.bind_wait_us = parse_scalar(key, value)?,
            "commit_csv_ppm" => self.commit_csv_ppm = parse_scalar(key, value)?,
            _ => return Err(ConfigError(format!("unknown key {key}"))),
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.workers == 0 || self.workers > 64 {
            return Err(ConfigError("workers must be 1..=64".into()));
        }
        if !(0.0..=1000.0).contains(&self.theta) {
            return Err(ConfigError("theta out of range".into()));
        }
        if !(0.0..=1.0).contains(&self.hint_alpha) {
            return Err(ConfigError("hint_alpha must be in [0, 1]".into()));
        }
        if self.max_incarnations == 0 {
            return Err(ConfigError("max_incarnations must be >= 1".into()));
        }
        if self.export_files == 0 {
            return Err(ConfigError("export_files must be >= 1".into()));
        }
        if self.out_ring && !(1..=16_384).contains(&self.out_ring_mb) {
            return Err(ConfigError("out_ring_mb must be 1..=16384".into()));
        }
        if self.out_ring && !self.out_token && self.out_owners.is_empty() {
            return Err(ConfigError("out_ring needs out_token or out_owners".into()));
        }
        if self.commit_threads == 0 || self.commit_threads > 64 {
            return Err(ConfigError("commit_threads must be 1..=64".into()));
        }
        if self.verify_sample_ppm > 1_000_000 {
            return Err(ConfigError("verify_sample_ppm must be <= 1000000".into()));
        }
        if self.bind_wait_us > 100_000 {
            return Err(ConfigError("bind_wait_us must be <= 100000".into()));
        }
        if self.commit_csv_ppm > 1_000_000 {
            return Err(ConfigError("commit_csv_ppm must be <= 1000000".into()));
        }
        if self.follow_wait_ms > 10_000 {
            return Err(ConfigError("follow_wait_ms must be <= 10000".into()));
        }
        if self.input_dual && self.ring_path.is_none() {
            return Err(ConfigError("input = dual needs ring_path".into()));
        }
        if self.input_dual && self.ingest_spin_us < 1_000_000 {
            return Err(ConfigError(
                "input = dual needs ingest_spin_us >= 1000000 (the ring is polled)".into(),
            ));
        }
        Ok(())
    }

    /// Read the config named by `AGAVE_FAST_LANE_CONFIG`, if set.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        let Ok(path) = std::env::var(CONFIG_ENV) else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|err| ConfigError(format!("reading {path}: {err}")))?;
        Self::parse(&text).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_defaults_and_overrides() {
        let text = r#"
            # fast lane
            enabled = true
            workers = 3
            worker_cores = [23, 47, 22]
            shared_cores = [0-2, 5]
            sched_core = none
            theta = 0.5
            speculation = off
            export_dir = "/tmp/fl"
        "#;
        let config = Config::parse(text).unwrap();
        assert!(config.enabled);
        assert_eq!(config.workers, 3);
        assert_eq!(config.worker_cores, vec![23, 47, 22]);
        assert_eq!(config.shared_cores, vec![0, 1, 2, 5]);
        assert_eq!(config.sched_core, None);
        assert_eq!(config.theta, 0.5);
        assert!(!config.speculation);
        assert_eq!(config.export_dir, Some(PathBuf::from("/tmp/fl")));
        assert_eq!(config.max_incarnations, 3);
    }

    #[test]
    fn test_parse_rejects_unknown_and_bad() {
        assert!(Config::parse("bogus = 1").is_err());
        assert!(Config::parse("workers = 0").is_err());
        assert!(Config::parse("enabled = maybe").is_err());
        assert!(Config::parse("worker_cores = 1,2").is_err());
        assert!(Config::parse("input = ring").is_err());
        assert!(Config::parse("input = dual\nring_path = /dev/shm/x").is_err());
        assert!(
            Config::parse("input = dual\nring_path = /dev/shm/x\ningest_spin_us = 1000000")
                .unwrap()
                .input_dual
        );
        assert!(Config::parse("").unwrap().enabled.eq(&false));
    }

    #[test]
    fn test_parse_out_ring() {
        let config = Config::parse(
            r#"
            out_ring = true
            out_ring_mb = 64
            out_owners = ["pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA", 11111111111111111111111111111111]
            "#,
        )
        .unwrap();
        assert!(config.out_ring && config.out_token);
        assert_eq!(config.out_ring_mb, 64);
        assert_eq!(config.out_owners.len(), 2);
        assert_eq!(config.out_owners[1], Pubkey::default());
        assert_eq!(
            config.out_ring_path,
            PathBuf::from("/dev/shm/fastlane.out.ring")
        );
        assert!(Config::parse("out_owners = [notakey]").is_err());
        assert!(Config::parse("out_ring = true\nout_token = false").is_err());
        assert!(Config::parse("out_ring = true\nout_ring_mb = 0").is_err());
    }

    /// The staged FRA config for the rebase build (`fast_lane.rebase.toml`) parses.
    #[test]
    fn test_parse_fra_rebase_config() {
        let config = Config::parse(FRA_REBASE_TOML).unwrap();
        assert!(config.rebase && config.eager_reexec && config.speculation);
        assert!(config.vm_opts);
        assert!(!Config::parse("").unwrap().vm_opts);
        assert_eq!(config.workers, 8);
        assert_eq!(config.theta, 0.5);
        assert!(config.chain && config.out_ring && config.input_dual);
        assert_eq!(config.mem_cap_mb, 4096);
        assert!(!Config::parse("").unwrap().rebase);
        assert!(Config::parse("rebase = maybe").is_err());
    }

    /// The staged FRA config for execute-once milestone 1 (`fast_lane.commit_shadow.toml`).
    #[test]
    fn test_parse_fra_commit_shadow_config() {
        let config = Config::parse(include_str!("fra_commit_shadow.toml")).unwrap();
        assert_eq!(config.commit, crate::control::COMMIT_SHADOW);
        assert!(config.rebase && config.vm_opts && config.chain && config.out_ring);
        assert_eq!(Config::parse("").unwrap().commit, crate::control::COMMIT_OFF);
        assert!(Config::parse("commit = maybe").is_err());
        assert_eq!(
            Config::parse("commit = off").unwrap().commit,
            crate::control::COMMIT_OFF
        );
    }

    /// The staged FRA config for execute-once milestone 2 (`fast_lane.commit_on.toml`).
    #[test]
    fn test_parse_fra_commit_on_config() {
        let config = Config::parse(include_str!("fra_commit_on.toml")).unwrap();
        assert_eq!(config.commit, crate::control::COMMIT_ON);
        assert_eq!(config.commit_threads, 3);
        assert!(config.commit_cores.is_empty());
        assert_eq!(config.verify_sample_ppm, 500);
        assert_eq!(config.follow_wait_ms, 100);
        assert_eq!(config.follow_spin_us, 200);
        assert!(config.rebase && config.vm_opts && config.chain && config.out_ring);
        assert!(Config::parse("commit_threads = 0").is_err());
        assert!(Config::parse("verify_sample_ppm = 1000001").is_err());
        let m3 = Config::parse("commit = on\ncommit_cores = [17-19]\ncommit_spin_us = 1000000")
            .unwrap();
        assert_eq!(m3.commit_cores, vec![17, 18, 19]);
    }

    /// The staged FRA config for the commit-latency build (`fast_lane.commit_latency.toml`).
    #[test]
    fn test_parse_fra_commit_latency_config() {
        let config = Config::parse(include_str!("fra_commit_latency.toml")).unwrap();
        assert_eq!(config.commit, crate::control::COMMIT_ON);
        assert_eq!(config.sample_mode, solana_runtime::fast_lane_commit::SAMPLE_FL);
        assert_eq!(config.bind_wait_us, 2000);
        assert!(!config.commit_on_workers);
        assert_eq!(config.commit_csv_ppm, 100_000);
        assert_eq!(config.verify_sample_ppm, 10_000);
        assert!(Config::parse("commit_on = workers").unwrap().commit_on_workers);
        assert!(Config::parse("commit_on = both").is_err());
        assert!(Config::parse("sample_mode = random").is_err());
        assert!(Config::parse("bind_wait_us = 100001").is_err());
    }

    const FRA_REBASE_TOML: &str = r#"
# Fast lane shadow #4 (FRA): the live shadow #3 config (/home/sol/fast_lane.toml, 2026-09-28
# 15:25Z) + `vm_opts = true` (result-identical VM shortcuts on FL's executor threads only,
# DESIGN §20) + `rebase = true` (delta-rebase value prediction for fee-payer / fee-sink
# chains, DESIGN §19). Nothing else changed.
# Binary: /home/sol/fl-bin/agave-validator-f33fbabe35 (fast-lane f33fbabe35 = c3f25234d5 + rebase
# + FL-only VM shortcuts).
# The older binaries reject these keys (unknown keys fail the config parse): deploy binary
# and config together. Runtime A/B without restart: `vm_opts=off|on`, `rebase=off|on` in the
# control file.
# Install: copy to /home/sol/fast_lane.toml (validator.sh exports AGAVE_FAST_LANE_CONFIG).
# Layout (48 logical CPUs, SMT sibling of N is N+24):
#   agave shared threads 0-7,24-31 · FL sched 8, ingest 9 (siblings 32,33 idle)
#   FL workers 10-14 + 36-38 (K=8; siblings 34,35 idle) · replay handlers 15-21, scheduler 22 (siblings idle)
#   geyserbench sampler 23 · agave receive chain 47
# Input: dual (proxy shared-memory ring v2 + blockstore), ring from proxy fl-ring-v2 7311fee.
# Runtime control: echo disable|enable|theta=X > /home/sol/fast_lane/fast_lane.ctl

enabled = true

workers = 8
worker_cores = [10, 11, 12, 13, 14, 36, 37, 38]
sched_core = 8
ingest_core = 9
shared_cores = [0-7, 24-31]
nice = 5
aux_nice = 10
spin_us = 1000000
worker_spin_us = 1000000
ingest_spin_us = 1000000

input = dual
ring_path = "/dev/shm/shredstream.v2.ring"

# Phase 2b: start a slot on FL's own complete run of its parent while agave has not frozen
# the parent yet (P's freeze-time writes and C's SlotHashes are resolved when agave freezes P).
# Set false to get shadow #2 behaviour with the new binary.
chain = true

# Phase 3: publish every FINAL transaction's account updates into a shared-memory ring for
# consumers (geyserbench `fastlane_ring`, later the bot). Accounts: the transaction's
# grouped-notification accounts owned by SPL Token / Token-2022 (`out_token`) or by an
# `out_owners` program; transactions without one are not published. Written by the
# coordinator (core 8) at FINAL; cost reported in `fast_lane_out` log lines every 10 s.
out_ring = true
out_ring_path = "/dev/shm/fastlane.out.ring"
out_ring_mb = 256
out_token = true
out_owners = []

speculation = true
theta = 0.5
max_incarnations = 3
eager_reexec = true
# Delta rebase (needs eager_reexec): predicted outputs for chains through fee payers and fee
# sinks, and rebase-aware speculation gating. Exactness does not depend on it (predictions are
# never final or emitted). Runtime toggle: rebase=on|off in the control file.
rebase = true
# FL executor threads run agave's SOLANA_VM_HEAP_ZERO_OPT / _PDA_CACHE / _SER_POOL shortcuts
# (result-identical; per-thread pools, `mem_vm_pool_kb`); agave's own threads are unchanged.
# Runtime toggle: vm_opts=on|off in the control file.
vm_opts = true

parent_wait_ms = 300
max_runs = 4
# Hard cap on FL-held memory (overlays, comparator frames/records, queues, pending entries,
# hints; `mem_*` in fast_lane_summary). Above it FL disables itself and releases everything
# (stays off until `echo enable > fast_lane.ctl`); new runs stop at half of it. Normal
# operation is expected well below 4 GiB; lower it once `mem_total_mb` has been observed.
mem_cap_mb = 4096

comparator = true
export_dir = "/home/sol/fast_lane"
control_file = "/home/sol/fast_lane/fast_lane.ctl"
export_file_mb = 256
export_files = 8
export_votes = false
mismatch_samples_per_min = 20
summary_interval_s = 10
"#;
}
