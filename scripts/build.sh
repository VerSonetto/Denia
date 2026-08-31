#!/usr/bin/env bash
# dsh-rs one-shot build: kill running instances, build the web console,
# release-build the server, and install the global `dsh-rs` command.
#
#   scripts/build.sh          build + install
#   scripts/build.sh --run    build + install, then start the server
set -euo pipefail
cd "$(dirname "$0")/.."

log() { printf '\n==> %s\n' "$*"; }

log "killing running dsh-rs processes (exe is locked while running)"
case "${OSTYPE:-}" in
  msys* | cygwin* | win32*)
    taskkill //IM dsh-rs.exe //F >/dev/null 2>&1 || true
    ;;
  *)
    pkill -x dsh-rs 2>/dev/null || true
    ;;
esac
sleep 1

log "building web console"
if [[ ! -d web/node_modules ]]; then
  (cd web && pnpm install)
fi
(cd web && pnpm build)

log "release build + global install"
cargo install --path crates/server --force

log "done"
echo "start with: dsh-rs        (console on http://127.0.0.1:3600)"
if [[ "${1:-}" == "--run" ]]; then
  exec dsh-rs
fi
