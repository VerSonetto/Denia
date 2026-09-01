#!/usr/bin/env bash
# denia one-shot build: kill running instances, build the web console,
# release-build the server, and install the global `denia` command.
#
#   scripts/build.sh          build + install
#   scripts/build.sh --run    build + install, then start the server (foreground)
#   scripts/build.sh --run-bg build + install, then start in background
set -euo pipefail
cd "$(dirname "$0")/.."

log() { printf '\n==> %s\n' "$*"; }

log "killing running denia processes (exe is locked while running)"
case "${OSTYPE:-}" in
  msys* | cygwin* | win32*)
    taskkill //IM denia.exe //F >/dev/null 2>&1 || true
    ;;
  *)
    pkill -x denia 2>/dev/null || true
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
echo "start with: denia        (console on http://127.0.0.1:3600)"
if [[ "${1:-}" == "--run" ]]; then
  exec denia
fi
if [[ "${1:-}" == "--run-bg" ]]; then
  case "${OSTYPE:-}" in
    msys* | cygwin* | win32*)
      powershell.exe -NoProfile -Command "Start-Process denia -WindowStyle Hidden"
      ;;
    *)
      nohup denia >/dev/null 2>&1 &
      ;;
  esac
  echo "server started in background — http://127.0.0.1:3600/"
fi
