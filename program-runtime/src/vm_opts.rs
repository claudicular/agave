//! Opt-in, result-identical implementation shortcuts for the SBF execution path.
//!
//! Every switch in this module changes only *how* the runtime reaches a result, never the result
//! itself: programs observe identical memory, compute units, logs, return data and account state
//! with the switch on or off, so a node can flip one without affecting consensus. Each switch is
//! controlled by an environment variable that is read once per process (on first use) and
//! defaults to off, which keeps the stock code path. A value of `1`, `true`, `yes` or `on`
//! (case-insensitive) enables it.
//!
//! [`EnvFlag::set`] exists for tests and benchmarks that compare both paths inside one process;
//! it is safe to flip at any time because every switch is designed to be correct when the flag
//! changes between the two halves of an operation (for example between handing out and
//! returning a pooled buffer).

use std::sync::atomic::{AtomicU8, Ordering};

const UNINITIALIZED: u8 = 0;
const DISABLED: u8 = 1;
const ENABLED: u8 = 2;

/// A boolean switch backed by an environment variable that is read once.
pub struct EnvFlag {
    name: &'static str,
    description: &'static str,
    state: AtomicU8,
}

impl EnvFlag {
    pub const fn new(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            description,
            state: AtomicU8::new(UNINITIALIZED),
        }
    }

    /// Name of the controlling environment variable.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Whether the switch is on. The first call reads the environment variable.
    #[inline]
    pub fn enabled(&self) -> bool {
        match self.state.load(Ordering::Relaxed) {
            ENABLED => true,
            DISABLED => false,
            _ => self.init_from_env(),
        }
    }

    /// Forces the switch on or off, overriding the environment variable.
    ///
    /// Intended for tests and benchmarks that run both code paths in one process.
    #[doc(hidden)]
    pub fn set(&self, enabled: bool) {
        self.state
            .store(if enabled { ENABLED } else { DISABLED }, Ordering::Relaxed);
    }

    #[cold]
    fn init_from_env(&self) -> bool {
        let enabled = std::env::var(self.name)
            .map(|value| parse_bool_env_flag(&value))
            .unwrap_or(false);
        let new_state = if enabled { ENABLED } else { DISABLED };
        match self.state.compare_exchange(
            UNINITIALIZED,
            new_state,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                log::info!(
                    "{}: {} ({})",
                    self.description,
                    if enabled { "enabled" } else { "disabled" },
                    self.name
                );
                enabled
            }
            // Another thread initialized it (or a test forced it) first.
            Err(current) => current == ENABLED,
        }
    }
}

/// Parses the boolean spelling accepted by all `SOLANA_VM_*` switches.
pub fn parse_bool_env_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `SOLANA_VM_HEAP_ZERO_OPT`: when returning a VM heap to the thread-local pool, zero only the
/// prefix that was mapped into the VM (`heap_size` from the transaction's compute budget, 32 KiB
/// by default) instead of the full 256 KiB allocation. See [`crate::mem_pool`].
pub static HEAP_ZERO_OPT: EnvFlag = EnvFlag::new(
    "SOLANA_VM_HEAP_ZERO_OPT",
    "vm heap reset: mapped prefix only",
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bool_env_flag() {
        for value in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(parse_bool_env_flag(value), "{value}");
        }
        for value in ["", "0", "false", "off", "no", "2", "enabled"] {
            assert!(!parse_bool_env_flag(value), "{value}");
        }
    }

    #[test]
    fn test_env_flag_reads_env_once_and_can_be_forced() {
        static FLAG: EnvFlag = EnvFlag::new("SOLANA_VM_TEST_ONLY_FLAG_UNSET", "test flag");
        // The variable is never set, so the flag starts off.
        assert!(!FLAG.enabled());
        FLAG.set(true);
        assert!(FLAG.enabled());
        FLAG.set(false);
        assert!(!FLAG.enabled());
    }
}
