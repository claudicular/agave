# AGENTS.md

This file provides guidance to coding agents working in this repository. It uses `CLAUDE.md` as input and incorporates explicit fork intent from the fork owner.

## Repository Reality (Evidence-Checked)

- Project: Agave validator client (Rust monorepo).
- Workspace size: 140 crates (`Cargo.toml` `[workspace].members` count).
- Active branch context in this checkout: `fast_arb` (contains latency instrumentation commits and `CLAUDE.md` updates).
- Global compile policy: warnings are denied (`Cargo.toml` -> `[workspace.lints.rust] warnings = "deny"`).

## Fork Intent (Authoritative)

For this fork, the primary objective is arbitrage performance. Treat this as the top-level optimization goal for modifications in this repository:

- Minimize end-to-end detection and reaction latency.
- Prioritize shred-path and replay/geyser-path observability for faster opportunity detection.
- Assume Jito/ShredStream-style delivery paths are valid operating assumptions for design and optimization choices in this fork.

When tradeoffs appear, optimize for measurable latency wins first, while preserving correctness and consensus safety requirements.

## Assumption Audit (What to Trust, What to Challenge)

1. "Arbitrage detection latency is the core motivation for all optimizations." -> **Accepted for this fork**
- This is fork-owner intent and must be treated as authoritative operating context for agent work in this repo.
- Upstream docs (`README.md`, `docs/src`, `CONTRIBUTING.md`) still define baseline quality, safety, and contribution standards.
- Agent rule: optimize for arbitrage latency within those safety constraints.

2. "Jito ShredStream/deshred flow is a core architectural path in this repo." -> **Accepted as fork assumption**
- Jito/ShredStream references are maintained as intentional assumptions for this fork's optimization strategy.
- Deshredding primitives are present in-tree (`ledger/src/shredder.rs`) and can be used as the technical substrate for shred-first analysis paths.

3. "Early speculative per-transaction notifier exists in this branch." -> **Incorrect for this branch**
- No `accounts-db/src/early_account_notifier.rs` exists here.
- `core/src/validator.rs` has no `early_account_notifier` field in `ValidatorConfig`.
- `core/src/replay_stage.rs::mark_dead_slot()` notifies slot-dead via `slot_status_notifier` and RPC updates, not a separate early notifier API.

4. "400ms slot and ~20ms micro-batches are fixed timing assumptions." -> **Use as optimization model, not protocol invariant**
- `DEFAULT_MS_PER_SLOT` is 400ms (used broadly).
- Tick math in `core/src/validator.rs` indicates 64 ticks/slot, i.e. 6.25ms per tick.
- QUIC coalescing default is 5ms (`streamer/src/quic.rs`: `DEFAULT_TPU_COALESCE`).
- Agent rule: it is valid to use micro-batch timing heuristics for latency budgeting in this fork, but avoid hardcoding brittle constants into consensus or protocol logic.

5. "RPC nodes are validators with voting disabled." -> **Mostly true, with nuance**
- `--no-voting` sets `voting_disabled` (`validator/src/commands/run/execute.rs`).
- `core/src/validator.rs` clears `authorized_voter_keypairs` when voting is disabled.
- TPU and TVU are still instantiated in validator startup (`core/src/validator.rs`).
- `--restricted-repair-only-mode` additionally removes TPU/TVU/repair addresses from advertised contact info (`validator/src/commands/run/execute.rs`), but is not a generic "turn everything off" mode.

6. "Unused timing variables are present intentionally for potential latency analysis." -> **Verified**
- `_load_and_execute_us` in `runtime/src/bank.rs`
- `_before_lock_us` and `_before_verify_us` in `ledger/src/blockstore_processor.rs`
- `_signal_received` in `core/src/replay_stage.rs`

## Latency Instrumentation Actually Present (fast_arb branch)

Current `fast_geyser_latency` events are emitted at these points:

- Account lock complete: `ledger/src/blockstore_processor.rs`
- Signature verification complete: `ledger/src/blockstore_processor.rs`
- Transaction execution complete: `svm/src/transaction_processor.rs`
- Geyser account update notify: `accounts-db/src/accounts_db/geyser_plugin_utils.rs`

This is metric instrumentation, not a standalone speculative notifier subsystem.

## Architecture Map (Grounded in Code + Docs)

- TPU (transaction intake/forward/produce): `core/src/tpu.rs`, docs in `docs/src/validator/tpu.md`
- TVU (shred receive/retransmit/replay): `core/src/tvu.rs`, docs in `docs/src/validator/tvu.md`
- Runtime/Bank execution + commit path: `runtime/src/bank.rs`
- SVM transaction processor: `svm/src/transaction_processor.rs`
- Ledger/blockstore replay and verification: `ledger/src/blockstore_processor.rs`
- Validator assembly/wiring: `core/src/validator.rs`

The two-pipeline model (TPU/TVU) described in `CLAUDE.md` is consistent with `docs/src/validator/anatomy.md` and validator startup wiring.

## Non-Negotiable Engineering Constraints

- Consensus-affecting changes must be feature-gated and have SIMD alignment (`CONTRIBUTING.md`).
- Add/modify feature gates in `feature-set/src/lib.rs` and use the `feature-gate` PR label.
- Keep warnings/clippy clean (warnings denied globally; CI enforces clippy/fmt/checks).
- Do not introduce `dev-context-only-utils` as a normal dependency unless explicitly justified and CI-compatible (`scripts/check-dev-context-only-utils.sh`).

## Build, Test, and Validation Commands

Prefer the repo wrapper:

```bash
./cargo build
./cargo test -p solana-core
./cargo test --all --tests --exclude solana-local-cluster
./scripts/cargo-clippy.sh
./cargo nightly fmt --all
```

Closest CI entry points:

```bash
ci/test-checks.sh
ci/test-stable.sh
```

Notes:
- `scripts/cargo-clippy.sh` uses nightly and repo-specific lockfile handling.
- CI checks nightly `frozen-abi`/`dummy-for-ci-check` paths and lockfile consistency via `scripts/cargo-for-all-lock-files.sh`.

## PR and Change Strategy for Agents

- Prefer small, reviewable changes; separate refactors from behavioral changes (`CONTRIBUTING.md`).
- Add tests for new or changed behavior (unit/integration as appropriate).
- For performance claims, include benchmark evidence (micro + relevant integration data).
- When touching consensus/runtime behavior, explicitly call out safety, feature-gate plan, and rollback strategy.

## Agent Operating Rules (This Repo)

- Treat fork-owner intent in this file and `CLAUDE.md` as authoritative for prioritization.
- Verify behavior in code before implementing assumptions in production paths.
- When making architecture/perf claims, cite at least one source file path in your rationale.
- Optimize for measured latency gains while preserving correctness, consensus safety, and testability.
