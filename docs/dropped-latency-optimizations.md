# Dropped Latency Optimizations (v3.1 → v4.0.0 rebase)

When the `add_tx_with_accounts_geyser` work was rebased from a v3.1.10 base onto
the **v4.0.0** release, a set of replay/geyser **latency optimizations** was
intentionally **dropped**. They were tightly coupled to v3.1-era internals that
v4.0 restructured (most importantly the Alpenglow replay-loop changes and the
`Blockstore::get_slot_entries_in_block` signature change), so re-applying them
verbatim was not safe. They will need to be **re-designed** against the v4.x
architecture rather than ported.

This document records what they were so the intent is not lost.

## Where the original code lives

The optimizations were implemented in these commits on the pre-rebase branch
(preserved as **`backup/pre-v4-rebase`**):

| Commit | Summary |
|--------|---------|
| `2e75cdda3e` | Add replay/geyser latency optimizations and agent guidance |
| `6aa220a785` | Fix v3.1.8 rebase drift in replay timing and run args |

To see any optimization in full:

```bash
git show 2e75cdda3e -- <file>
git diff v3.1.10 backup/pre-v4-rebase -- <file>
```

## Fork intent (why these existed)

The fork's top-level goal is **arbitrage detection/reaction latency**: detect
account/transaction state changes from incoming micro-batches as early as
possible and react before competitors. Each optimization below shaved latency
off the shred → replay → geyser-notify path. See `CLAUDE.md` (“Solana Arbitrage
& Detection Speed”) for the full rationale.

---

## 1. Replay fast-ingress control-loop skip

- **CLI:** `--replay-fast-ingress`, `--replay-control-loop-ms` (default `50`)
- **Config:** `ValidatorConfig::replay_fast_ingress: bool`, `replay_control_loop_ms: u64`
- **Files:** `core/src/replay_stage.rs`, `core/src/validator.rs`, run `args.rs`/`execute.rs`

**Mechanism:** In the replay loop, the expensive “control path” (fork choice,
vote/reset selection, stats, leader scheduling, duplicate/repair handling) ran
every iteration. With fast-ingress enabled, the control path was throttled to at
most once per `replay_control_loop_ms`; on the skipped iterations the loop did
nothing but wait briefly on the ledger signal and replay active banks — getting
freshly-replayed bank state (and therefore geyser notifications) out sooner.
New `ReplayTiming` counters `fast_ingress_skipped_control_count` and
`fast_ingress_wait_receive_us` tracked it.

**Why it needs rethinking for 4.x:** v4.0 introduced the **Alpenglow** consensus
migration, which restructured this exact region of the replay loop
(`enable_alpenglow`, `migration_status.is_ready_to_enable()`). The old
unconditional control-path skip cannot be dropped in without reasoning about how
it interacts with Alpenglow migration/their new scheduling. This is the single
biggest re-design item.

## 2. Replay hot cache for completed-data entries

- **CLI:** `--replay-hot-cache-mb` (size in MB; `0` disables)
- **Config:** `ValidatorConfig::replay_hot_cache_mb: usize`
- **Files:** `ledger/src/blockstore.rs`, `core/src/validator.rs`

**Mechanism:** Added a `completed_data_entries_cache` (`RwLock<CompletedDataEntriesCache>`)
to `Blockstore`, sized via `set_replay_hot_cache_capacity_bytes()`. The
`get_slot_entries_in_block` path checked the cache first (hit counter
`blockstore-replay-hot-cache-hit`) and `cache_completed_data_block_entries()`
populated it, avoiding repeated deserialization of recently-completed entry
ranges during replay.

**Why it needs rethinking for 4.x:** v4.0 changed
`Blockstore::get_slot_entries_in_block` to take a generic `deserialize` closure
and return `Vec<T>` (it previously returned `Vec<Entry>`). The caching layer was
built around the old signature/return type and must be re-fitted (cache
key/value types, where the hit/miss check wraps the new generic path).

## 3. TVU shred sigverify threading / batching

- **CLI:** `--tvu-shred-sigverify-max-batches`, `--tvu-shred-sigverify-max-age-us`
  (plus larger `tvu_shred_sigverify_threads`)
- **Config:** `ValidatorConfig::tvu_shred_sigverify_threads`,
  `tvu_shred_sigverify_max_batches`, `tvu_shred_sigverify_max_age_us`
- **Files:** `core/src/tvu.rs`, `core/src/window_service.rs`, `turbine/src/sigverify_shreds.rs`

**Mechanism:** Increased shred sigverify parallelism and the number of batches
drained per iteration, and added a max-age bound, so inbound shreds were
verified and handed to the window service with less queuing delay.

**Why it needs rethinking for 4.x:** these are mostly additive knobs, but the
v4.0 defaults and sigverify wiring changed (`replay_transactions_threads`,
`tvu_shred_sigverify_threads` defaults were re-tuned upstream). Re-introduce as
tuning on top of the v4.0 thread model rather than the v3.1 values.

## 4. Async accounts-update notifier dispatch

- **CLI:** `--accounts-notify-async`
- **Config:** `ValidatorConfig::accounts_notify_async: bool`
- **Files:** `geyser-plugin-manager/src/accounts_update_notifier.rs`

**Mechanism:** Added an optional `AsyncAccountsDispatch` — a bounded channel
(capacity 16,384) feeding a dedicated dispatch thread — so geyser account-update
notifications were handed off instead of being delivered synchronously on the
commit path. This kept the commit/replay path from blocking on slow plugins.

**Why it needs rethinking for 4.x:** the notifier struct/`new()` were refactored
upstream. More importantly, async dispatch trades latency-on-the-hot-path for
**notification ordering/freshness** guarantees; for arbitrage we likely want a
re-evaluation of whether async (or a different hand-off) is still the right call
under the v4.x notifier. **Note:** the kept transaction-grouped notification
feature is currently delivered **synchronously**; if we re-introduce async, it
should cover the grouped path too.

## 5. `--latency-mode` umbrella flag

- **CLI:** `--latency-mode` (`standard` | `hybrid-speculative`, default `standard`)
- **Config:** `ValidatorConfig::latency_mode: LatencyMode` (enum in `core/src/validator.rs`)
- **Files:** `core/src/validator.rs`, run `args.rs`/`execute.rs`

**Mechanism:** A single switch intended to select a coherent bundle of the above
latency behaviors (e.g. `hybrid-speculative` turning on the aggressive paths).

**Why it needs rethinking for 4.x:** it only makes sense once the optimizations
it gates are re-designed; reintroduce alongside them.

---

## What was *kept* in the v4.0.0 rebase

- **Geyser transaction-grouped account notifications** (the
  `notify_transaction_accounts` interface + `bank.rs` commit-flow hook).
- **`--enable-transaction-accounts-notify`** gate for the above (default off).
- **RPC pre-simulation accounts** for `simulateTransaction`.

The gating flag was deliberately **disentangled** from the dropped async/latency
scaffolding it originally sat on top of.
