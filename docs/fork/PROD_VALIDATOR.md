# Production validator (FRA): branches, deploy, upgrade

The one place that says what runs in production and how to change it. Keep it current: any
change to the node's setup lands here and in `fork/fra/` in the same commit.

## What runs

FRA = `sol@64.130.41.179` (EPYC 9275F, 48 logical CPUs, 251 GB RAM), a non-voting RPC node.
arb_bot (`central-pie.service`, CPUs 15-23/39-47) reads its Yellowstone stream on
`:10000` and uses its RPC on `:8899`.

| Piece | Where on the node | Source of truth |
|---|---|---|
| Validator | `/home/sol/agave-bin/current/agave-validator` | claudicular/agave **`prod/v4.4.1`** |
| Yellowstone plugin | `/home/sol/agave-bin/current/libyellowstone_grpc_geyser.so` | claudicular/yellowstone-grpc **`prod/geyser-v17.0.1-agave-4.4.1`** |
| Release dirs | `/home/sol/agave-bin/<id>/` with `BUILD_INFO`; `current`, `previous` symlinks | `fork/fra/install.sh` |
| Launcher | `/home/sol/bin/validator.sh` (all flags and env) | `fork/fra/validator.sh` |
| Units | `/etc/systemd/system/agave.service`, `agave-cpu-layout.service` (no drop-ins) | `fork/fra/*.service` |
| CPU layout | `/home/sol/agave-cpu-layout.env`, `/home/sol/bin/pin_layout.py` | `fork/fra/` |
| Fast lane config | `/home/sol/fast_lane.toml` | `fast-lane/fra.toml` |
| Yellowstone config | `/home/sol/yellowstone-config.json` | `fork/fra/yellowstone-config.json` |
| Runtime switches | `/home/sol/fast_lane/fast_lane.ctl` (`enable`/`disable`/`commit=...`), `/home/sol/receive_diet.ctl` (`all`) | applied live |
| Checkouts (build here) | `/home/sol/agave`, `/home/sol/yellowstone-grpc` | the two prod branches |
| Log | `/home/sol/solana-rpc.log` (a cron truncates it daily at 02:00 UTC) | |
| Shredstream proxy | screen `shredstream`, `/home/sol/shredstream-proxy` (not part of this doc) | |

The validator and plugin are released together because a Rust Geyser plugin must be built
against the identical interface crate and toolchain (`rust-toolchain.toml`).

## Known state (2026-11-16)

- **The fast lane runs on Alpenglow** (mainnet since slot 459,648,000 = 2026-10-21 13:36Z;
  from then until this release FL refused every slot, skip reason `alpenglow`).
  - It executes every normal Alpenglow slot. The child Clock is the parent's (upstream
    `update_clock_slot_for_alpenglow`), and `alpenglow_migration_succeeded` follows the parent.
  - Still refused: the migration and the first Alpenglow block (`alpenglow_migration`); with
    `alpenglow_fast_leader_handover` active, the first slot of each leader window
    (`fast_leader_handover`), because those may carry UpdateParent; epoch boundaries and reward
    slots as before. A ring or blockstore UpdateParent marker skips the slot (`update_parent`).
  - **No chaining** (`chain = false`): a child of an unfrozen Alpenglow parent needs the
    parent's footer (Clock, nanosecond clock, vote rewards). It waits for the parent's freeze
    instead (`chained_alpenglow` if forced). Phase 2: parse the parent's footer.
  - The slot check skips the footer-written Clock, nanosecond clock and vote accounts.
  - First live shadow run (2026-11-16 22:40Z+): ~650k transactions full-compared, 0 mismatches.
    Steady state FL p50/p90 0.9/2.5 ms vs agave 4.4/13 ms from shred arrival.
  - `commit = on` since 2026-11-17 00:12Z: ~14k FL commits per 10 s, 0 errors, every footer
    matched. Shred arrival -> Yellowstone `transaction_accounts` (geyserbench, 20k Tokenkeg
    txs): p50 0.96 / p95 3.40 / p99 7.46 ms.
- **Safety net under Alpenglow.** The TowerBFT net (gossip votes, duplicate confirmation, dump
  and repair) is dead. The reference hash is the **leader's block-footer `bank_hash`**,
  compared at freeze: the leader's claim, not a ≥52%-stake confirmation, but detected at
  freeze instead of ~130 ms later.
  - A footer mismatch on a bank FL committed into is not marked dead. FL is poisoned
    provisionally, the bank is cleared and the slot replayed once agave-only. That replay's
    footer check decides: match → `fl_wrong` (sticky until restart); still differs with FL's
    hash → `leader_footer` (poison lifted, slot dead as upstream); differs with another hash →
    `fl_wrong`, slot dead; cleared, dead first or another block replayed → `unresolved`
    (sticky).
  - A mismatch on a bank with no FL commits is upstream behaviour (`not_fl`, FL untouched).
  - Every matching footer advances `fast_lane_cluster_check` (`consensus=alpenglow`) and the
    out_ring rollback horizon; vote and gossip-lag fields are omitted there, not zero.
  - Every Alpenglow bank clear (UpdateParent, abandoned bank, switch) first stops the bank's
    board; FL treats it as a dead slot (ROLLBACK; the slot and its children are skipped).
- Everything else in the fork is active: grouped notifications, RPC changes, no-coalesce,
  scheduler spin, VM shortcuts, receive diet, Yellowstone delivery runtime.
- 4.4.1 rejects `--limit-blockstore-size` below 100,000,000 (4.3 accepted 50M).

## Branch model

Each prod branch is **one upstream release tag + a short series of topical commits** (agave:
9, see `CLAUDE.md` "Fork features"; Yellowstone: 3). Fixes are folded into their topical commit
(`git commit --fixup` + `GIT_SEQUENCE_EDITOR=true git rebase -i --autosquash <tag>`), so an
upgrade rebases ~9 commits, never a long history. Old prod states are tagged
(`prod-v4.3.0-final` = the v4.3.0 build that ran 2026-09-29..11-06, including the three
dropped replay switches).

## Deploy a build

All building happens on the node (never on a laptop). **Keep builds off FL's and
Yellowstone's cores** (8-14, 31, 36-38): FL's executors spin there, and a compile on those
cores stalled FL by up to ~350 ms (2026-11-16). Use `taskset -c 30,32-35 nice -n 19 cargo ...`
(or wrap the build lines below), or briefly `echo disable > /home/sol/fast_lane/fast_lane.ctl`.

```bash
# 1. Push the branch(es) from the laptop, then on FRA:
cd /home/sol/agave && git fetch origin && git checkout prod/v4.4.1 && git reset --hard origin/prod/v4.4.1
nohup taskset -c 30,32-35 nice -n 19 ~/.cargo/bin/cargo build --release --bin agave-validator > ~/build-agave.log 2>&1 &
cd /home/sol/yellowstone-grpc && git fetch origin && git reset --hard origin/prod/geyser-v17.0.1-agave-4.4.1
nohup taskset -c 30,32-35 nice -n 19 ~/.cargo/bin/cargo build --release -p yellowstone-grpc-geyser > ~/build-ys.log 2>&1 &
# 2. Package and activate (takes effect at the next restart):
~/bin/agave-install.sh release            # prints the release id
~/bin/agave-install.sh activate <id>
~/bin/agave-install.sh config             # only when fork/fra/* or fast-lane/fra.toml changed
# 3. If the node is far behind or was down: fresh snapshot first (see below).
# 4. Restart and wait for health + catch-up:
~/bin/restart_check.sh 1800               # add --rollback to fall back to `previous` on failure
```

After a deploy, check for at least 90 minutes: `systemctl show agave -p NRestarts`,
`dmesg | grep -i oom`, RSS (`systemctl status agave`, ~150-175 GB is normal), and the
fast-lane summary lines (`grep -a fast_lane_commit /home/sol/solana-rpc.log | tail -1`:
`committed` large, `failed=0`; `fast_lane_summary ... mismatch=0`). A healthy catch-up alone is
not enough (the 2026-09-28 leak only showed after an hour).

## Snapshot (node behind or down)

`/home/sol/bin/dl_snapshot.sh` (non-interactive; run under `nohup ... > ~/snapshot-download.log
&`) downloads a recent full + incremental snapshot into `/mnt/ledger` at 30-100 MB/s
(~45 min for the ~125 GB full; a rerun fetches only a newer incremental, so rerun right before
the restart). validator.sh uses `--no-snapshot-fetch`: agave boots from the highest-slot
archives in `/mnt/ledger`, so delete stale ones. A fresh snapshot is healthy in ~5-10 minutes;
booting from a days-old or bloated local snapshot can take hours.

## Rollback

- Fast lane only: `echo disable > /home/sol/fast_lane/fast_lane.ctl` (agave executes
  everything, no restart). `echo enable` turns it back on.
- Binary: `~/bin/agave-install.sh rollback && ~/bin/restart_check.sh`. A release older
  than a mandatory upgrade (below) cannot follow the chain; rolling back across one is useless.

## Upgrading to a new upstream release

Mandatory upgrades are real: a feature activation that the running version does not know
stops replay at the activating slot. On 2026-11-06 13:34Z feature
`loaderv3_programdata_size_floor` (`8whkDe...`) activated at slot 466,128,000 (epoch 1079)
and the v4.3.0 fork stood still there for 10 days. Watch Anza's release notes and mainnet's
`getVersion`, and keep FRA within one minor version of mainnet.

1. Fetch tags (`git fetch upstream --tags`). Trial the squash-merge to see conflicts without a
   worktree: `git merge-tree --write-tree --name-only --merge-base=<old-tag> <new-tag> prod/<old>`.
2. Branch `prod/<new-tag>` from the new tag and cherry-pick the old prod commits one at a time
   (`git cherry-pick <sha>`, `git config rerere.enabled true`). Resolve each in its own
   commit; regenerate the four lockfiles with `cargo metadata` (CLAUDE.md "Gotchas").
3. Drop what upstream now does itself (v4.4: the mapped-prefix heap reset), and say so in the
   commit message.
4. Audit what the fast lane mirrors from agave, even where it auto-merged:
   `runtime/src/bank/check_transactions.rs` (FL calls its pieces), fee distribution and
   freeze-time writes (`Bank::fee_deposit_accounts`), `Bank::verify_transaction` (size and
   version rules), sysvar handling, program cache API, the unified scheduler's task handler,
   the transaction type replay uses, and new consensus features (diff
   `feature-set/src/lib.rs` between the tags; check whether any changes locking or account
   loading, e.g. implicit read locks).
5. Build and run the test battery on the node (below). Then port Yellowstone: rebase its 3
   commits onto the upstream tag built for the new agave (`v<N>+solana.<agave>` or
   `geyser-v<N>`), point `[patch.crates-io] agave-geyser-plugin-interface` at the new prod
   branch, check `yellowstone-grpc-proto/proto/geyser.proto` stays wire-compatible with
   arb_bot (field 100 `transaction_accounts`).
6. Deploy as above. Update this doc and CLAUDE.md's branch name in the same series.

## Tests

On the node, `--release` unless noted (each line is one cargo invocation):

```
-p agave-fast-lane --no-fail-fast            # lib + differential/commit/control/cluster tests
-p solana-runtime --lib -- fast_lane          # FL bank accessors, claim board
-p solana-runtime --lib -- grouped_transaction_accounts
-p solana-geyser-plugin-manager --features agave-unstable-api
-p solana-core --lib -- fast_lane             # safety net e2e
-p solana-core --lib -- --ignored fast_lane_real_commit
-p solana-core --lib -- shred_fetch window_service completed_data_sets
-p solana-unified-scheduler-pool              # NOT under taskset (see below)
-p solana-turbine --lib -- sigverify receive_diet retransmit cluster_nodes
-p solana-ledger --lib -- sigverify_shreds
-p solana-program-runtime --lib               # debug, not --release (debug_assert tests)
-p solana-svm --test vm_opts_differential
-p solana-rpc --lib -- account_scans simulate_transaction
```

v4.4.1 baseline (2026-11-16): all pass (fast lane 53 lib + 7 integration; core FL e2e 2;
grouped notifications 17; plugin manager 41; scheduler 36; turbine 32; program-runtime 49
fork-module tests in debug; vm_opts differential 9; RPC 14).
Alpenglow FL (2026-11-17): fast lane 63 lib+integration, runtime FL 22, core FL 5 + real
commit 1, replay/repair/consensus 414, plugin manager 24.

Run test binaries under the same `taskset -c 30,32-35 nice -n 19` as builds, except the
scheduler suite: under any `taskset` mask `test_scheduler_active_after_stale` and both
`test_scheduler_schedule_execution_failure_*` wait forever (same on the pre-Alpenglow build);
unrestricted they pass in 0.01 s and use no measurable CPU.
