# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## This repository: the claudicular production fork

This is **claudicular/agave**, our fork of Anza's Agave validator. It runs on the FRA
non-voting RPC node (sol@64.130.41.179), which feeds arb_bot's Geyser stream and RPC.

- **Official branch:** `prod/v<upstream-tag>`; today **`prod/v4.4.1`** = upstream `v4.4.1`
  + 9 topical fork commits. The deployed build, its config and the upgrade procedure are in
  [`docs/fork/PROD_VALIDATOR.md`](docs/fork/PROD_VALIDATOR.md). **Read it before any work that
  will be deployed: a validator upgrade, a new fork feature, or touching FRA.**
- Everything else (`master`, `fast-lane`, `replay-chain-diet`, `add_tx_with_accounts_geyser`,
  ...) is history. The last v4.3.0 production build is tag `prod-v4.3.0-final`.
- Node-side files (launcher, units, CPU layout, configs, install/restart scripts) are
  versioned in [`fork/fra/`](fork/fra/); the fast lane's production config is
  [`fast-lane/fra.toml`](fast-lane/fra.toml). Change them here, then install them on the node.

### Hard rules

- **Never build agave (or the Yellowstone plugin) on a laptop.** No `cargo build`,
  `cargo check` or `cargo test` locally: one `target/` is 5-30 GB. Git work and
  `cargo metadata` lockfile regeneration are fine. Push a branch, then build and test on the
  node (`/home/sol/agave`, ~2-5 min incremental, under `nohup` with a log).
- Never `pkill -f <pattern>` on the node: agave's command line contains `solana-rpc.log`
  and similar strings, and a pattern match killed production once. Kill by PID.
- The node runs agave in exactly one way: `agave.service` -> `/home/sol/bin/validator.sh`
  -> `/home/sol/agave-bin/current/agave-validator`. No systemd drop-ins, no ad-hoc launch
  scripts, no binaries run from a build tree.

## Repository Overview

Agave is the Anza validator client for the Solana blockchain, a high-performance blockchain implementation. This is a large Rust monorepo with ~140 workspace crates.

## Solana Arbitrage & Detection Speed

> **CRITICAL:** This is the core motivation for all optimizations in this project. Every speed improvement made to this validator must keep arbitrage detection latency in mind.

### Why Speed Matters

Solana operates in **400ms slots**, but within each slot the leader validator broadcasts **micro batches (entries)** approximately every **~20ms**. For arbitrage:

1. **Detect** transaction and account state changes from incoming micro batches as fast as possible
2. **React** by sending arbitrage transactions to land in the earliest subsequent micro batch
3. **Win** by being first - the first valid arbitrage transaction captures the opportunity

The difference between detecting an opportunity in batch N and landing your transaction in batch N+1 vs N+3 is the difference between profit and getting beaten by a competitor.

### Detection Methods: Latency Comparison

```
Jito ShredStream Proxy (deshred):
  Shreds from ShredStream → Deshred/reconstruct → Extract transactions → DETECT
  Latency: ~minimal, processes shreds as they arrive

Geyser Plugin (standard):
  Shreds arrive → Window Service → Blockstore → Replay Stage → Execute → Commit → DETECT
  Latency: Full replay pipeline, notifications only after execution and storage
```

**ShredStream advantage:** Transactions are detected directly from shreds as they're received, before the full replay pipeline. This can provide a significant head start over Geyser-based detection.

### Shred Delivery: Paid Services vs Turbine

In Turbine, shreds propagate through a stake-weighted tree - high-stake validators receive shreds first, and low/no-stake RPC nodes are at the bottom, receiving shreds last. For arbitrage, this latency is unacceptable.

**Paid shred streaming services** (like Jito ShredStream) solve this by:
1. Aggregating shreds directly from high-stake validators (top of Turbine tree)
2. Sending shreds directly to subscribers, bypassing the Turbine tree entirely

This means an unstaked arbitrage node receives shreds at nearly the same time as the highest-stake validators, rather than waiting for propagation through multiple Turbine layers.

### Micro Batch Timing

```
Slot (400ms)
├── Entry batch 0   (t=0ms)    ← Detect here
├── Entry batch 1   (t=20ms)   ← Land arb tx here (ideal)
├── Entry batch 2   (t=40ms)   ← Still good
├── Entry batch 3   (t=60ms)   ← Competitor may beat you
│   ...
└── Entry batch 19  (t=380ms)
```

Every ~20ms of detection latency saved is one more batch earlier you can potentially land your arbitrage transaction.

### Optimization Priorities

When making changes to this validator, consider impact on:
1. **Shred processing latency** - How fast can we extract transaction data from incoming shreds?
2. **Account state detection** - How quickly can we determine account state changes?
3. **Transaction submission** - How fast can we get arbitrage transactions to the leader?

This applies to replay stage optimizations, Geyser improvements, shred processing, and any other changes that touch the transaction detection or submission path.

## Build Commands

Run these **on the node** (see Hard rules), in `/home/sol/agave`:

```bash
# Production validator binary (what install.sh packages)
cargo build --release --bin agave-validator

# Tests for a specific crate / a single test
cargo test --release -p solana-core test_name

# Run clippy (uses nightly, matches CI)
./scripts/cargo-clippy.sh

# Format code (formatting is fine locally)
./cargo nightly fmt
```

## Architecture Overview

The validator uses a **pipelined architecture** inspired by CPU design, with two main pipelines:

### TPU (Transaction Processing Unit)
Handles incoming transactions (runs on all nodes, produces blocks only when leader):
1. **QUIC Receive** - Receives transactions from clients via QUIC, stake-weighted rate limiting
2. **SigVerify Stage** - Deduplicates packets, filters invalid signatures
3. **Banking Stage** - Executes transactions against Bank state, produces entries
4. **Broadcast Stage** - Packages entries into shreds (erasure-coded fragments), broadcasts via Turbine

### TVU (Transaction Validation Unit)
Receives, validates, and stores blocks (runs on all nodes):
1. **ShredFetch Stage** - Receives shreds from network
2. **Retransmit Stage** - Forwards shreds to downstream peers in Turbine tree
3. **Window Service** - Reconstructs blocks from shreds using FEC
4. **Replay Stage** - Replays transactions, maintains forks, implements Tower BFT consensus
5. **Voting Service** - Creates and broadcasts votes

### Key Components

- **core/** - Main validator logic: TPU, TVU, banking stage, replay stage, consensus
- **runtime/** - Bank (account state at a slot), transaction execution environment
- **svm/** - Solana Virtual Machine: transaction processor, program execution
- **ledger/** - Blockstore: persistent shred storage, block reconstruction
- **gossip/** - Peer discovery and cluster information propagation (control plane)
- **turbine/** - Block propagation via hierarchical multi-layer tree structure
- **programs/** - Native programs (stake, vote, system, BPF loader)
- **accounts-db/** - Account storage and indexing

### Data Flow
```
Leader:   Network → SigVerify → Banking Stage → SVM execution → Broadcast → Turbine tree
Validator: Turbine → Retransmit → Window → Blockstore → Replay Stage → Fork choice → Vote
```

### Fork features (on `prod/v4.4.1`)

One commit each, in this order. All are off unless enabled; FRA's settings are in
`fork/fra/validator.sh` and `fast-lane/fra.toml`.

| Commit | Enable | What |
|---|---|---|
| rpc: pre-simulation accounts | always | `simulateTransaction` returns requested accounts as they were *before* the simulated tx (arb_bot relies on this) |
| rpc: refuse accounts scans | `--rpc-allow-account-scans` re-enables | GPA, getLargestAccounts, getSupply, getTokenLargestAccounts, getTokenAccountsBy* return -32090 (a stuck scan once pinned clean and bloated FRA) |
| geyser: transaction-grouped account notifications | `--enable-transaction-accounts-notify` | see below; feeds Yellowstone's `transaction_accounts` subscription |
| shred fetch: no 5 ms coalesce | always | TVU/repair sockets hand each read downstream at once |
| unified scheduler spin | `SOLANA_UNIFIED_SCHEDULER_SPIN_US` | bounded spin before a handler parks |
| vm shortcuts | `SOLANA_VM_PDA_CACHE`, `_SER_POOL`, `_CHEAP_TIMERS` | result-identical execution shortcuts (`program-runtime/src/vm_opts.rs`); the former `_HEAP_ZERO_OPT` is upstream since v4.4 (#14929) |
| receive diet | `SOLANA_RECEIVE_DIET` (+ `_FILE` for runtime control) | exact trims on the shred receive path (serial sigverify for small batches, leaf retransmit, skip empty data sets) |
| fast lane | `AGAVE_FAST_LANE_CONFIG` | speculative executor that executes each tx once and commits into agave's banks ("execute once"); crate `fast-lane/`. Runs on Alpenglow banks (no chaining yet); safety reference is the leader's block-footer bank hash, see `docs/fork/PROD_VALIDATOR.md` "Known state" |
| docs | - | this file, `docs/fork/`, `fork/fra/` |

**Transaction-grouped account updates.** One Geyser notification per transaction with every
writable account it touched (untouched write-locked accounts skipped) plus optional read-only
accounts of listed owners (Token mints). It carries signature, slot, index and the committing
bank's `bank_id` (under Alpenglow an UpdateParent can clear a bank; consumers drop updates
whose `bank_id` matches `notify_entry_update_parent`'s `cleared_bank_id`). Each group reserves
its own block of `AccountsDb::write_version`, so `(slot, write_version)` is a valid staleness
guard. Delivery is synchronous on the commit path. Since v4.4 the plugin interface is a
crates.io crate (`agave-geyser-plugin-interface` 5.0.0); the fork **vendors** it as
`geyser-plugin-interface/` (published source + the three callbacks) and the Yellowstone fork
patches to it. Files: `geyser-plugin-interface/src/geyser_plugin_interface.rs`,
`accounts-db/src/accounts_update_notifier_interface.rs`,
`geyser-plugin-manager/src/accounts_update_notifier.rs`, `runtime/src/bank.rs`
(`notify_transaction_accounts_to_plugins`).

**Fast lane.** FL ingests the shredstream proxy's shared-memory ring and the blockstore,
executes on pinned spinning workers with Block-STM-style speculation, and commits each final
result through agave's own commit path (`runtime/src/transaction_execution.rs`
`commit_external`, claim board in `runtime/src/fast_lane_commit.rs`); agave's replay follows
and executes what FL does not commit. Safety: every slot's bank hash is checked (under
Alpenglow against the leader's block-footer hash at freeze; mismatch on an FL bank -> FL
poisoned, slot replayed without FL, verdict attributed), 0.1% sampled re-execution with
full comparison, hard memory cap. Anything FL mirrors from agave lives in
`runtime/src/bank/fast_lane.rs` and must track upstream changes on every rebase (tx checks,
fee distribution, sysvars, program cache). Design history: arb_bot
`docs/research/shred-to-geyser-2026-09-27/fast_lane/`.

Dropped earlier: the v3.1-era replay optimizations (`docs/dropped-latency-optimizations.md`),
and on the v4.4.1 rebase three replay switches never enabled in production (pipelined entry
submission, parallel sanitize helpers, fast pickup; still on `prod-v4.3.0-final`).

### RPC Nodes vs Validators

RPC nodes are **not a separate code path** - they are validators with voting disabled (`--no-voting` flag). Both TPU and TVU still run:

| Component | RPC Node (--no-voting) | Full Validator |
|-----------|------------------------|----------------|
| **TVU** | Yes - must replay blocks to serve RPC queries | Yes |
| **TPU** | Yes - receives transactions | Yes |
| **Voting** | No - authorized_voter_keypairs cleared | Yes |
| **Block Production** | No - not scheduled as leader | Yes |
| **Transaction Forwarding** | Yes - forwards to current leader | Yes |

**Why TVU runs on RPC nodes:** The Bank (account state) must be kept current by replaying all blocks. Without this, RPC methods like `getBalance` or `getAccountInfo` would return stale data.

**Why TPU runs on RPC nodes:** RPC nodes receive `sendTransaction` requests. The TPU's FetchStage receives these, and the ForwardingStage forwards them to the current leader's `tpu_forwards` address.

**Restricted repair-only mode** (`--restricted-repair-only-mode`): More restrictive than `--no-voting`. Removes TPU/TVU ports from gossip announcements entirely, limiting the node to only repairing its ledger.

## Code Conventions

### Clippy Lints (enforced in CI)
- `--deny=clippy::arithmetic_side_effects` - Use checked/saturating arithmetic
- `--deny=clippy::default_trait_access` - Use `Type::default()` not `Default::default()`
- `--deny=clippy::manual_let_else` - Use `let-else` syntax
- `--deny=clippy::used_underscore_binding` - Don't use underscore-prefixed bindings

### Disallowed Methods (see clippy.toml)
- `std::net::UdpSocket::bind` - Use `solana_net_utils::bind_*` instead
- `tokio::net::UdpSocket::bind` - Use `solana_net_utils::bind_*_async` instead
- `lazy_static!` - Use `std::sync::LazyLock` or `OnceLock` instead

### Feature Flags
- `dev-context-only-utils` - Test utilities that should only be dev-dependencies. CI verifies this isn't used in production code paths.
- `frozen-abi` - ABI stability checking (nightly only)

### Feature Gates
Consensus-breaking changes must be behind feature gates. Add the `feature-gate` label to PRs that add/modify feature gates. Feature gates are defined in `feature-set/src/lib.rs`.

## Testing Patterns

```bash
# Run tests excluding local-cluster (faster)
./cargo test --all --tests --exclude solana-local-cluster

# Run with specific log level
RUST_LOG=solana_core=debug ./cargo test -p solana-core test_name

# Run local cluster tests (slow, integration tests)
./cargo test -p solana-local-cluster
```

Gotchas:
- Four Cargo workspaces/lockfiles: `./Cargo.lock`, `dev-bins/`, `programs/sbf/`,
  `ci/xtask/`. After a dependency change regenerate each with
  `cargo metadata --manifest-path <ws>/Cargo.toml --format-version 1 >/dev/null`.
- `solana-geyser-plugin-manager` is behind `agave-unstable-api`: test it with
  `--features agave-unstable-api` or it runs 0 tests.
- `#[should_panic]` tests that rely on `debug_assert!` fail under `--release`; run those
  crates (e.g. `solana-program-runtime`) in debug.
- Fork test battery for a rebase: `docs/fork/PROD_VALIDATOR.md` section "Tests".

## PR Guidelines

- Small, frequent PRs preferred (~1000 lines max for functional changes)
- Use Draft PRs to iterate before requesting review
- Consensus-breaking changes require a merged SIMD (Solana Improvement Document)
- All changes should have tests covering 90%+ of new code paths
- Don't mix refactoring and logical changes in the same PR

## Plans
- at the end of each plan, give me a list of unresolved questions to answer, if any.
- choose plan names according to what they're about. use three/four words with hyphens.
