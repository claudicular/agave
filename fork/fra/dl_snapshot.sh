#!/bin/bash
# Fetch a fresh mainnet snapshot into /mnt/ledger (c29r3/solana-snapshot-finder, ~30-100 MB/s).
# Use when the node is far behind (booting from its own old snapshot would take hours to catch
# up) or after an outage. A second run fetches only a newer incremental. Agave boots from the
# highest-slot full + incremental in /mnt/ledger (validator.sh runs with --no-snapshot-fetch).
# Non-interactive (no -it), so it can run under nohup/screen. Log: /home/sol/snapshot-download.log
set -euo pipefail
exec docker run --rm --name snapfinder \
    -v /mnt/ledger:/mnt/ledger/snapshot --user "$(id -u):$(id -g)" \
    c29r3/solana-snapshot-finder:latest --snapshot_path /mnt/ledger/snapshot "$@"
