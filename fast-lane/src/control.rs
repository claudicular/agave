//! Process-wide on/off state, runtime tunables and the control file.
//!
//! `FL_ACTIVE` is checked (relaxed) by every tap and tee: when false they are no-ops.
//! `FL_POISONED` is set by a contained panic and keeps the fast lane off until the process
//! restarts (phase 1 never re-arms itself).

use {
    log::{error, info, warn},
    std::{
        path::Path,
        sync::{
            OnceLock,
            atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
        },
    },
};

static FL_ACTIVE: AtomicBool = AtomicBool::new(false);
static FL_POISONED: AtomicBool = AtomicBool::new(false);
static FL_VM_OPTS: AtomicBool = AtomicBool::new(false);
static FL_COMMIT_MODE: AtomicU8 = AtomicU8::new(COMMIT_OFF);

/// `commit = off`: FL results stay inside FL (shadow comparator, output ring).
pub const COMMIT_OFF: u8 = 0;
/// `commit = shadow` (milestone 1): FL keeps each transaction's full processing result and
/// agave hands the fast lane a copy of its own (`solana_runtime::fast_lane_commit`); the
/// comparator checks them field by field. Nothing is committed.
pub const COMMIT_SHADOW: u8 = 1;

pub fn parse_commit_mode(value: &str) -> Option<u8> {
    match value {
        "off" => Some(COMMIT_OFF),
        "shadow" => Some(COMMIT_SHADOW),
        _ => None,
    }
}

pub fn commit_mode_name(mode: u8) -> &'static str {
    match mode {
        COMMIT_SHADOW => "shadow",
        _ => "off",
    }
}

#[inline]
pub fn commit_mode() -> u8 {
    FL_COMMIT_MODE.load(Ordering::Relaxed)
}

/// Set the commit mode; agave's side follows while the fast lane is active.
pub fn set_commit_mode(mode: u8) {
    FL_COMMIT_MODE.store(mode, Ordering::SeqCst);
    sync_runtime_mode();
}

/// Agave's side of the commit mode: the fast lane's mode while it is active and not
/// poisoned, else off (agave stops capturing immediately).
fn sync_runtime_mode() {
    let mode = if FL_ACTIVE.load(Ordering::SeqCst) && !FL_POISONED.load(Ordering::SeqCst) {
        commit_mode()
    } else {
        COMMIT_OFF
    };
    solana_runtime::fast_lane_commit::set_mode(mode);
}

/// How agave's replay executes transactions: with a `TransactionStatusSender` it records
/// logs, inner instructions, return data and balances, and caps logs at
/// `log_messages_bytes_limit`. FL executes identically when it keeps processing results.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgaveExecution {
    pub record: bool,
    pub log_messages_bytes_limit: Option<usize>,
}

static AGAVE_EXECUTION: OnceLock<AgaveExecution> = OnceLock::new();

pub fn set_agave_execution(execution: AgaveExecution) {
    let _ = AGAVE_EXECUTION.set(execution);
}

pub fn agave_execution() -> AgaveExecution {
    AGAVE_EXECUTION.get().copied().unwrap_or_default()
}

/// Whether FL keeps each transaction's full processing result (shadow check or commit).
#[inline]
pub fn keep_processed() -> bool {
    commit_mode() != COMMIT_OFF
}

/// Result-identical VM shortcuts (`solana_program_runtime::vm_opts`: mapped-prefix heap reset,
/// PDA on-curve cache, pooled program-input buffers) on the fast lane's executor threads only;
/// agave's own threads keep the process-wide setting. Workers apply it before each execution.
#[inline]
pub fn vm_opts() -> bool {
    FL_VM_OPTS.load(Ordering::Relaxed)
}

pub fn set_vm_opts(on: bool) {
    FL_VM_OPTS.store(on, Ordering::Relaxed);
}

/// Whether taps/tees should forward events and FL threads should work.
#[inline]
pub fn is_active() -> bool {
    FL_ACTIVE.load(Ordering::Relaxed)
}

pub fn is_poisoned() -> bool {
    FL_POISONED.load(Ordering::Relaxed)
}

/// Turn the fast lane on or off. Refused (returns false) once poisoned.
pub fn set_active(active: bool) -> bool {
    if active && is_poisoned() {
        return false;
    }
    FL_ACTIVE.store(active, Ordering::SeqCst);
    sync_runtime_mode();
    true
}

/// Permanently disable the fast lane (contained panic, fatal internal error, a safety check).
/// Agave's side stops at once: no more captures, and (commit mode) no more waiting on FL.
pub fn poison(reason: &str) {
    FL_POISONED.store(true, Ordering::SeqCst);
    FL_ACTIVE.store(false, Ordering::SeqCst);
    sync_runtime_mode();
    error!("fast lane: disabled permanently: {reason}");
}

/// Runtime-tunable scheduler knobs (control file `key=value`).
pub struct Tunables {
    speculation: AtomicBool,
    eager_reexec: AtomicBool,
    theta_bits: AtomicU32,
    max_incarnations: AtomicU32,
    rebase: AtomicBool,
}

impl Tunables {
    pub fn new(speculation: bool, eager_reexec: bool, theta: f32, max_incarnations: u32) -> Self {
        Self {
            speculation: AtomicBool::new(speculation),
            eager_reexec: AtomicBool::new(eager_reexec),
            theta_bits: AtomicU32::new(theta.to_bits()),
            max_incarnations: AtomicU32::new(max_incarnations),
            rebase: AtomicBool::new(false),
        }
    }
    /// Delta-rebase value prediction and rebase-aware speculation (`sched`): off by default.
    pub fn with_rebase(self, rebase: bool) -> Self {
        self.set_rebase(rebase);
        self
    }
    pub fn rebase(&self) -> bool {
        self.rebase.load(Ordering::Relaxed)
    }
    pub fn set_rebase(&self, v: bool) {
        self.rebase.store(v, Ordering::Relaxed)
    }
    pub fn speculation(&self) -> bool {
        self.speculation.load(Ordering::Relaxed)
    }
    pub fn eager_reexec(&self) -> bool {
        self.eager_reexec.load(Ordering::Relaxed)
    }
    pub fn theta(&self) -> f32 {
        f32::from_bits(self.theta_bits.load(Ordering::Relaxed))
    }
    pub fn max_incarnations(&self) -> u32 {
        self.max_incarnations.load(Ordering::Relaxed)
    }
    pub fn set_speculation(&self, v: bool) {
        self.speculation.store(v, Ordering::Relaxed)
    }
    pub fn set_theta(&self, v: f32) {
        self.theta_bits.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// Counters written by taps/tees on agave threads (atomics only).
#[derive(Default)]
pub struct TapStats {
    pub tap_batches: AtomicU64,
    pub tap_drops: AtomicU64,
    pub agave_frames: AtomicU64,
    pub agave_frame_drops: AtomicU64,
    pub events: AtomicU64,
    pub event_drops: AtomicU64,
}

impl TapStats {
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// One parsed control-file command.
#[derive(Debug, PartialEq)]
pub enum Command {
    Enable,
    Disable,
    Panic,
    Set(String, String),
}

pub fn parse_commands(text: &str) -> Vec<Command> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(|l| match l {
            "enable" => Command::Enable,
            "disable" | "pause" => Command::Disable,
            "panic" => Command::Panic,
            other => match other.split_once('=') {
                Some((k, v)) => Command::Set(k.trim().to_string(), v.trim().to_string()),
                None => Command::Set(other.to_string(), String::new()),
            },
        })
        .collect()
}

/// Apply the control file if it changed since the last poll. Returns `true` if the file
/// requested a panic drill (only honored when allowed; the caller panics on an FL thread).
pub fn poll_control_file(
    path: &Path,
    last_contents: &mut Option<String>,
    tunables: &Tunables,
    allow_panic: bool,
) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    if last_contents.as_deref() == Some(text.as_str()) {
        return false;
    }
    *last_contents = Some(text.clone());
    let mut panic_requested = false;
    for command in parse_commands(&text) {
        match command {
            Command::Enable => {
                if set_active(true) {
                    info!("fast lane: enabled by control file");
                } else {
                    warn!("fast lane: enable refused (poisoned; restart required)");
                }
            }
            Command::Disable => {
                set_active(false);
                info!("fast lane: disabled by control file");
            }
            Command::Panic => {
                if allow_panic {
                    panic_requested = true;
                } else {
                    warn!("fast lane: panic command ignored (allow_panic_command=false)");
                }
            }
            Command::Set(key, value) => match key.as_str() {
                "speculation" => {
                    tunables.set_speculation(matches!(value.as_str(), "true" | "on" | "1"))
                }
                "rebase" => tunables.set_rebase(matches!(value.as_str(), "true" | "on" | "1")),
                "vm_opts" => set_vm_opts(matches!(value.as_str(), "true" | "on" | "1")),
                "commit" => match parse_commit_mode(value.as_str()) {
                    Some(mode) => {
                        set_commit_mode(mode);
                        info!("fast lane: commit={} by control file", commit_mode_name(mode));
                    }
                    None => warn!("fast lane: bad commit mode {value}"),
                },
                "theta" => match value.parse::<f32>() {
                    Ok(theta) if (0.0..=1000.0).contains(&theta) => tunables.set_theta(theta),
                    _ => warn!("fast lane: bad theta {value}"),
                },
                _ => warn!("fast lane: unknown control command {key}={value}"),
            },
        }
    }
    panic_requested
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_commands() {
        let cmds = parse_commands("enable\n# c\n theta = 0.3 \ndisable\npanic\n");
        assert_eq!(
            cmds,
            vec![
                Command::Enable,
                Command::Set("theta".into(), "0.3".into()),
                Command::Disable,
                Command::Panic
            ]
        );
    }

    #[test]
    fn test_rebase_control_command() {
        let dir = std::env::temp_dir().join(format!("fl_ctl_rebase_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fast_lane.ctl");
        let t = Tunables::new(true, true, 0.5, 3).with_rebase(true);
        assert!(t.rebase());
        let mut last = None;
        std::fs::write(&path, "rebase=off\n").unwrap();
        poll_control_file(&path, &mut last, &t, false);
        assert!(!t.rebase());
        std::fs::write(&path, "rebase = on\n").unwrap();
        poll_control_file(&path, &mut last, &t, false);
        assert!(t.rebase());
        let before = vm_opts();
        std::fs::write(&path, "vm_opts=on\n").unwrap();
        poll_control_file(&path, &mut last, &t, false);
        assert!(vm_opts());
        set_vm_opts(before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_commit_mode_control_command() {
        assert_eq!(parse_commit_mode("shadow"), Some(COMMIT_SHADOW));
        assert_eq!(parse_commit_mode("off"), Some(COMMIT_OFF));
        assert_eq!(parse_commit_mode("bogus"), None);
        let cmds = parse_commands("commit=shadow\n");
        assert_eq!(cmds, vec![Command::Set("commit".into(), "shadow".into())]);
    }

    #[test]
    fn test_tunables() {
        let t = Tunables::new(true, true, 0.2, 3);
        assert_eq!(t.theta(), 0.2);
        t.set_theta(0.5);
        assert_eq!(t.theta(), 0.5);
        t.set_speculation(false);
        assert!(!t.speculation());
    }
}
