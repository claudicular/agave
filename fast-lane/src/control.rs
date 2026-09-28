//! Process-wide on/off state, runtime tunables and the control file.
//!
//! `FL_ACTIVE` is checked (relaxed) by every tap and tee: when false they are no-ops.
//! `FL_POISONED` is set by a contained panic and keeps the fast lane off until the process
//! restarts (phase 1 never re-arms itself).

use {
    log::{error, info, warn},
    std::{
        path::Path,
        sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};

static FL_ACTIVE: AtomicBool = AtomicBool::new(false);
static FL_POISONED: AtomicBool = AtomicBool::new(false);

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
    true
}

/// Permanently disable the fast lane (contained panic, fatal internal error).
pub fn poison(reason: &str) {
    FL_POISONED.store(true, Ordering::SeqCst);
    FL_ACTIVE.store(false, Ordering::SeqCst);
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
    fn test_tunables() {
        let t = Tunables::new(true, true, 0.2, 3);
        assert_eq!(t.theta(), 0.2);
        t.set_theta(0.5);
        assert_eq!(t.theta(), 0.5);
        t.set_speculation(false);
        assert!(!t.speculation());
    }
}
