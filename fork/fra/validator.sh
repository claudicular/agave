#!/bin/bash
# FRA agave launcher: the only way agave is started (agave.service ExecStart). Installed as
# /home/sol/bin/validator.sh from claudicular/agave fork/fra/validator.sh; edit it there, then
# reinstall (docs/fork/PROD_VALIDATOR.md). No systemd drop-ins: every setting lives here.
set -euo pipefail

# The active release: /home/sol/agave-bin/<release-id>/ holds the validator and the Yellowstone
# plugin built for it (they must match); `current` points at it (fork/fra/install-release.sh).
RELEASE=/home/sol/agave-bin/current

# Fast lane (execute once). Runtime control: /home/sol/fast_lane/fast_lane.ctl.
export AGAVE_FAST_LANE_CONFIG=/home/sol/fast_lane.toml
# Result-identical latency switches (fork; each defaults to off).
export SOLANA_VM_PDA_CACHE=1
export SOLANA_VM_SER_POOL=1
export SOLANA_VM_CHEAP_TIMERS=1
export SOLANA_UNIFIED_SCHEDULER_SPIN_US=50
# Receive-path trims; the file is re-read every second (empty or missing = all off).
export SOLANA_RECEIVE_DIET=all
export SOLANA_RECEIVE_DIET_FILE=/home/sol/receive_diet.ctl

exec "$RELEASE/agave-validator" \
    --identity /home/sol/validator-keypair.json \
    --known-validator 7Np41oeYqPefeNQEHSv1UDhYrehxin3NStELsSKCT4K2 \
    --known-validator GdnSyH3YtwcxFvQrVVJMm1JhTS4QVX7MFsX56uJLUfiZ \
    --known-validator DE1bawNcRJB9rVm3buyMVfr8mBEoyyu73NBovf2oXJsJ \
    --known-validator CakcnaRDHka2gXyfbEd2d3xsvkJkqsLw2akB3zsN1D2S \
    --only-known-rpc \
    --full-rpc-api \
    --no-voting \
    --ledger /mnt/ledger \
    --accounts /mnt/accounts \
    --log /home/sol/solana-rpc.log \
    --rpc-port 8899 \
    --rpc-bind-address 0.0.0.0 \
    --private-rpc \
    --dynamic-port-range 8000-8100 \
    --entrypoint entrypoint.mainnet-beta.solana.com:8001 \
    --entrypoint entrypoint2.mainnet-beta.solana.com:8001 \
    --entrypoint entrypoint3.mainnet-beta.solana.com:8001 \
    --entrypoint entrypoint4.mainnet-beta.solana.com:8001 \
    --entrypoint entrypoint5.mainnet-beta.solana.com:8001 \
    --expected-genesis-hash 5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d \
    --wal-recovery-mode skip_any_corrupted_record \
    --limit-blockstore-size 100000000 \
    --no-snapshot-fetch \
    --no-skip-initial-accounts-db-clean \
    --geyser-plugin-config /home/sol/yellowstone-config.json \
    --no-xdp \
    --enable-transaction-accounts-notify \
    --unified-scheduler-handler-threads 2
