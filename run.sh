#!/usr/bin/env bash
# Dev-mode launcher for pilot. Prepends the vendored zig 0.15.2 to
# PATH (libghostty-vt's build.zig rejects newer zig) and forwards
# extra args to the binary (e.g. `./run.sh --fresh`).
#
# First-time: `make setup` to fetch zig.
# Day-to-day: `./run.sh [args…]` or `make run`.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "${ROOT}"

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux)  os=linux ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64)        arch=x86_64 ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac

ZIG_DIR="${ROOT}/vendor/zig/${arch}-${os}-0.15.2"
if [ ! -x "${ZIG_DIR}/zig" ]; then
  echo "vendored zig missing — run \`make setup\` first." >&2
  exit 1
fi

PATH="${ZIG_DIR}:${PATH}" cargo run -p pilot-tui -- "$@"
