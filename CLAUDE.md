# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

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

```bash
# Build (uses ./cargo wrapper which selects correct toolchain from rust-toolchain.toml)
./cargo build

# Build release (not for production validators - see docs/src/cli/install.md)
./cargo build --release

# Run all tests
./cargo test

# Run tests for a specific crate
./cargo test -p solana-core

# Run a single test
./cargo test -p solana-core test_name

# Run clippy (uses nightly, matches CI)
./scripts/cargo-clippy.sh

# Format code
./cargo nightly fmt

# Run benchmarks (requires nightly)
cargo +nightly bench
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

### Early Speculative Account Notifications

> **NOTE:** The early notifier code is only on the `early-notifier-per-tx` branch. It is intentionally kept off the `fast_arb` branch to keep that branch clean for production deployment.

A notification system that emits account updates immediately after each successful transaction execution, **before** storage commit. Notifications fire per-transaction inside the SVM loop, providing the lowest possible latency for account update notifications.

**Timing comparison:**
```
Geyser (existing):     For each tx: SVM execute → End batch → commit → store → NOTIFY
Early (per-tx):        For each tx: SVM execute → NOTIFY → End batch → commit → store
```

**Benefit:** Transactions earlier in a batch get notified much faster. With batches of 64 txs, the first tx's notification fires ~64x sooner than batch-level.

**Key files:**
- `accounts-db/src/early_account_notifier.rs` - Trait definition (`EarlyAccountUpdateNotifier`)
- `svm-callback/src/lib.rs` - `TransactionProcessingCallback.notify_early_account_update()` method
- `svm/src/transaction_processor.rs:500-519` - Per-transaction notification in SVM loop
- `runtime/src/bank.rs` - Bank implements callback, forwards to notifier
- `core/src/replay_stage.rs` - Slot invalidation handling in `mark_dead_slot()`
- `core/src/validator.rs` - `ValidatorConfig.early_account_notifier` field

**Usage:**
```rust
// Implement the trait
impl EarlyAccountUpdateNotifier for MyNotifier {
    fn notify_account_update(&self, slot: Slot, txn_signature: &Signature, accounts: &[(&Pubkey, &AccountSharedData)]);
    fn notify_slot_invalidated(&self, slot: Slot);  // Called when fork is abandoned
}

// Set on validator config
config.early_account_notifier = Some(Arc::new(MyNotifier::new()));
```

**Important:** These notifications are speculative. Consumers must handle `notify_slot_invalidated()` to revert state when forks are abandoned.

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

## PR Guidelines

- Small, frequent PRs preferred (~1000 lines max for functional changes)
- Use Draft PRs to iterate before requesting review
- Consensus-breaking changes require a merged SIMD (Solana Improvement Document)
- All changes should have tests covering 90%+ of new code paths
- Don't mix refactoring and logical changes in the same PR
