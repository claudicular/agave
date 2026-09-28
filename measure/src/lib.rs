#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]
/// Shared with `solana-svm-measure` (a single source file, compiled into both crates so that
/// neither depends on the other); each crate reads `SOLANA_VM_CHEAP_TIMERS` and calibrates once.
#[path = "../../svm-measure/src/clock.rs"]
pub mod clock;
pub mod macros;
pub mod measure;
