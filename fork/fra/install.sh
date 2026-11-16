#!/bin/bash
# FRA release/config installer for the claudicular/agave production fork. Run ON the node as sol.
# It never builds; build first in the node's checkouts (docs/fork/PROD_VALIDATOR.md):
#   /home/sol/agave             (claudicular/agave prod/<tag>)  -> target/release/agave-validator
#   /home/sol/yellowstone-grpc  (claudicular/yellowstone-grpc prod/...) -> libyellowstone_grpc_geyser.so
#
#   install.sh release            copy both artifacts into /home/sol/agave-bin/<id>/ (+ BUILD_INFO)
#   install.sh activate <id>      point agave-bin/current at <id> (old target kept as `previous`)
#   install.sh rollback           swap current and previous
#   install.sh config             install fork/fra/* + fast-lane/fra.toml from the agave checkout
#                                 (validator.sh, units, CPU layout, Yellowstone and FL configs)
#   install.sh status             show releases, current/previous and what the running agave uses
# Activation takes effect on the next agave restart (fork/fra/restart_check.sh).
set -euo pipefail
AGAVE_SRC=${AGAVE_SRC:-/home/sol/agave}
YS_SRC=${YS_SRC:-/home/sol/yellowstone-grpc}
BIN=/home/sol/agave-bin
FRA=$AGAVE_SRC/fork/fra

clean_head() { # repo -> short sha, refuses a dirty tree
  local repo=$1
  if [ -n "$(git -C "$repo" status --porcelain --untracked-files=no)" ]; then
    echo "error: $repo has uncommitted changes; commit and push them first" >&2; exit 1
  fi
  git -C "$repo" rev-parse --short=10 HEAD
}

case "${1:-}" in
release)
  a=$(clean_head "$AGAVE_SRC"); y=$(clean_head "$YS_SRC")
  v=$("$AGAVE_SRC/target/release/agave-validator" --version | awk '{print $2}')
  id="v$v-$a-ys$y"; dir=$BIN/$id
  [ -e "$dir" ] && { echo "$dir exists"; exit 1; }
  mkdir -p "$dir"
  install -m 755 "$AGAVE_SRC/target/release/agave-validator" "$dir/agave-validator"
  install -m 644 "$YS_SRC/target/release/libyellowstone_grpc_geyser.so" "$dir/libyellowstone_grpc_geyser.so"
  {
    echo "release: $id"; echo "installed: $(date -u +%FT%TZ)"
    echo "agave: $(git -C "$AGAVE_SRC" rev-parse HEAD) ($(git -C "$AGAVE_SRC" rev-parse --abbrev-ref HEAD))"
    echo "yellowstone: $(git -C "$YS_SRC" rev-parse HEAD) ($(git -C "$YS_SRC" rev-parse --abbrev-ref HEAD))"
    echo "version: $("$dir/agave-validator" --version)"
    sha256sum "$dir/agave-validator" "$dir/libyellowstone_grpc_geyser.so"
  } > "$dir/BUILD_INFO"
  cat "$dir/BUILD_INFO"; echo "next: $0 activate $id"
  ;;
activate)
  id=${2:?release id}; [ -x "$BIN/$id/agave-validator" ] || { echo "no release $id"; exit 1; }
  [ -L "$BIN/current" ] && ln -sfn "$(readlink "$BIN/current")" "$BIN/previous"
  ln -sfn "$id" "$BIN/current"; echo "current -> $id (previous -> $(readlink "$BIN/previous" 2>/dev/null || echo none))"
  ;;
rollback)
  p=$(readlink "$BIN/previous"); c=$(readlink "$BIN/current")
  ln -sfn "$p" "$BIN/current"; ln -sfn "$c" "$BIN/previous"; echo "current -> $p (previous -> $c)"
  ;;
config)
  clean_head "$AGAVE_SRC" >/dev/null
  install -m 755 "$FRA/validator.sh" /home/sol/bin/validator.sh
  install -m 755 "$FRA/pin_layout.py" /home/sol/bin/pin_layout.py
  install -m 755 "$FRA/dl_snapshot.sh" /home/sol/bin/dl_snapshot.sh
  install -m 755 "$FRA/restart_check.sh" /home/sol/bin/restart_check.sh
  install -m 755 "$FRA/install.sh" /home/sol/bin/agave-install.sh
  install -m 644 "$FRA/agave-cpu-layout.env" /home/sol/agave-cpu-layout.env
  install -m 644 "$FRA/yellowstone-config.json" /home/sol/yellowstone-config.json
  install -m 644 "$AGAVE_SRC/fast-lane/fra.toml" /home/sol/fast_lane.toml
  sudo -n install -m 644 "$FRA/agave.service" /etc/systemd/system/agave.service
  sudo -n install -m 644 "$FRA/agave-cpu-layout.service" /etc/systemd/system/agave-cpu-layout.service
  sudo -n systemctl daemon-reload
  echo "installed config from $AGAVE_SRC ($(git -C "$AGAVE_SRC" rev-parse --short=10 HEAD)); restart agave to apply"
  ;;
status)
  ls -1 "$BIN"; echo "current -> $(readlink "$BIN/current" 2>/dev/null)"; echo "previous -> $(readlink "$BIN/previous" 2>/dev/null)"
  pid=$(systemctl show -p MainPID --value agave); echo "running: pid $pid $(readlink -f /proc/$pid/exe 2>/dev/null)"
  if [ -d /etc/systemd/system/agave.service.d ]; then
    echo "WARNING: agave drop-ins present (there should be none):"; ls /etc/systemd/system/agave.service.d
  fi
  ;;
*) sed -n 2,14p "$0"; exit 2 ;;
esac
