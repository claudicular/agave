#!/usr/bin/env bash
# Restart agave.service and wait until it is healthy and caught up (FRA).
#   restart_check.sh [timeout-seconds] [--rollback]
# Caught up = local getHealth "ok" and local processed slot within 20 of mainnet's.
# A boot from a fresh snapshot (dl_snapshot.sh) is healthy in ~5-10 min; from a days-old local
# snapshot it can take hours, so download first. With --rollback, a timeout or a panic swaps
# agave-bin/current back to `previous` and restarts once more.
set -uo pipefail
TIMEOUT="${1:-1800}"; ROLLBACK="${2:-}"
LOG=/home/sol/solana-rpc.log
PUBLIC=https://api.mainnet-beta.solana.com
rpc() { curl -s -m 5 -X POST "$1" -H 'Content-Type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":[{\"commitment\":\"processed\"}]}"; }
slot() { rpc "$1" getSlot | python3 -c 'import sys,json; print(json.load(sys.stdin).get("result",0))' 2>/dev/null || echo 0; }
health() { curl -s -m 5 -X POST localhost:8899 -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' | grep -c '"ok"'; }

LOG_START=$(stat -c %s "$LOG" 2>/dev/null || echo 0)
echo "$(date -u +%T) restarting agave.service (release $(readlink /home/sol/agave-bin/current))"
sudo -n systemctl daemon-reload
sudo -n systemctl restart agave.service
START=$(date +%s)
while true; do
  sleep 15
  EL=$(( $(date +%s) - START ))
  PANICS=$(tail -c +"$LOG_START" "$LOG" 2>/dev/null | grep -a -c -i "panicked at")
  L=$(slot http://localhost:8899); P=$(slot "$PUBLIC"); H=$(health)
  echo "$(date -u +%T) +${EL}s health=$H local=$L public=$P lag=$((P - L)) panics=$PANICS restarts=$(systemctl show agave -p NRestarts --value)"
  if [ "$PANICS" -gt 0 ]; then echo "PANIC in log"; break; fi
  if [ "$H" = "1" ] && [ "$L" -gt 0 ] && [ $((P - L)) -lt 20 ]; then echo "CAUGHT UP after ${EL}s"; exit 0; fi
  if [ "$EL" -ge "$TIMEOUT" ]; then echo "TIMEOUT"; break; fi
done
if [ "$ROLLBACK" = "--rollback" ]; then
  /home/sol/bin/agave-install.sh rollback && sudo -n systemctl restart agave.service
  echo "rolled back to $(readlink /home/sol/agave-bin/current); check health manually"
fi
exit 1
