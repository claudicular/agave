//! Panic containment and thread placement.
//!
//! Agave's panic hook (`solana_metrics::set_panic_hook`) calls `process::exit(1)` inside
//! the hook, before unwinding, so `catch_unwind` alone cannot keep the validator alive.
//! [`install_panic_hook`] wraps whatever hook is installed: a panic on a thread marked
//! `FL_CONTAINED` is logged and poisons the fast lane, then unwinding proceeds to the
//! `catch_unwind` in [`run_contained`]; every other panic goes to agave's hook unchanged.
//!
//! What this cannot contain: a panic inside a syscall called from JIT-compiled code (JIT
//! frames have no unwind tables, so the process aborts), a double panic, and code that runs
//! on agave threads (the taps and tees are a few non-panicking lines each).

use {
    crate::control,
    log::error,
    std::{
        cell::Cell,
        panic::{self, AssertUnwindSafe},
        sync::Once,
        thread::{self, JoinHandle},
    },
};

thread_local! {
    static FL_CONTAINED: Cell<bool> = const { Cell::new(false) };
}

static INSTALL_HOOK: Once = Once::new();

/// Wrap the currently installed panic hook (idempotent). Must run after agave installs its
/// own hook (`solana_metrics::set_panic_hook` at validator startup).
pub fn install_panic_hook() {
    INSTALL_HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if FL_CONTAINED.try_with(|c| c.get()).unwrap_or(false) {
                let thread = thread::current();
                error!(
                    "fast lane: contained panic on thread {}: {info}",
                    thread.name().unwrap_or("?")
                );
                control::poison("contained panic");
                return;
            }
            previous(info)
        }));
    });
}

/// Mark the calling thread as a fast-lane thread (panics are contained).
pub fn mark_contained() {
    FL_CONTAINED.with(|c| c.set(true));
}

/// Run `f`, converting a panic into a poisoned (disabled) fast lane.
pub fn run_contained<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(_) => {
            control::poison(what);
            None
        }
    }
}

/// Where a fast-lane thread runs.
#[derive(Debug, Clone)]
pub enum Placement {
    /// Pin to these logical CPUs.
    Pinned(Vec<usize>),
    /// Unpinned (inherits the spawner's mask) at this nice value.
    Niced(i32),
}

#[cfg(target_os = "linux")]
fn apply_placement(placement: &Placement) {
    match placement {
        Placement::Pinned(cpus) => {
            let ids: Result<Vec<_>, _> = cpus
                .iter()
                .map(|cpu| agave_cpu_utils::CpuId::new(*cpu))
                .collect();
            match ids.and_then(|ids| agave_cpu_utils::set_cpu_affinity(None, ids)) {
                Ok(()) => {}
                Err(err) => log::warn!("fast lane: pinning to {cpus:?} failed: {err}"),
            }
        }
        Placement::Niced(nice) => {
            // Per-thread nice: on Linux setpriority(PRIO_PROCESS, tid) targets one task.
            // SAFETY: plain syscalls with scalar arguments.
            let rc = unsafe {
                let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
                libc::setpriority(libc::PRIO_PROCESS, tid, *nice)
            };
            if rc != 0 {
                log::warn!("fast lane: setpriority({nice}) failed");
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_placement(_placement: &Placement) {}

/// Spawn a contained fast-lane thread with the given placement.
pub fn spawn(
    name: &str,
    placement: Placement,
    f: impl FnOnce() + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    let name_owned = name.to_string();
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            mark_contained();
            apply_placement(&placement);
            run_contained(&format!("panic on {name_owned}"), f);
        })
}
