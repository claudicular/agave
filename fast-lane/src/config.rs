//! Fast-lane configuration.
//!
//! Read once at startup from the file named by `AGAVE_FAST_LANE_CONFIG`. The format is a
//! small TOML subset: one `key = value` per line, `#` comments, values are integers,
//! floats, booleans, quoted or bare strings, or `[a, b, c]` integer lists. Unknown keys are
//! rejected so typos are caught at boot.

use std::{fmt, path::PathBuf, str::FromStr};

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
    /// Give up on a slot whose parent does not freeze within this time.
    pub parent_wait_ms: u64,
    /// Maximum concurrently active runs.
    pub max_runs: usize,
    /// Pause when the overlays hold more than this many bytes of account data.
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
}
