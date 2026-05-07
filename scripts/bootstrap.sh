#!/usr/bin/env bash
# Bootstrap pilot's build environment.
#
# Idempotent. Installs:
#   - zig 0.15.2 to vendor/zig/<host>/. Used by libghostty's build.zig
#     (which rejects zig >= 0.16). The Makefile prepends this to PATH
#     so any system zig is ignored.
#
# libghostty-rs is vendored under crates/libghostty-vt* — no separate
# clone needed.
#
# After running, `make build` / `make run` work without the user
# having any specific zig version on PATH.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENDOR="${ROOT}/vendor"
ZIG_VERSION="0.15.2"

# ── Detect host ─────────────────────────────────────────────────────────
case "$(uname -s)" in
  Darwin)  os=macos ;;
  Linux)   os=linux ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64)        arch=x86_64 ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac
host="${arch}-${os}"

# ── Install zig 0.15.2 ──────────────────────────────────────────────────
zig_dir="${VENDOR}/zig/${host}-${ZIG_VERSION}"
zig_bin="${zig_dir}/zig"

if [ -x "${zig_bin}" ]; then
  echo "zig ${ZIG_VERSION}: already at ${zig_bin}"
else
  echo "downloading zig ${ZIG_VERSION} for ${host}..."
  mkdir -p "${VENDOR}/zig"
  url="https://ziglang.org/download/${ZIG_VERSION}/zig-${arch}-${os}-${ZIG_VERSION}.tar.xz"
  tmp="$(mktemp -d)"
  trap "rm -rf ${tmp}" EXIT
  curl -fsSL "${url}" -o "${tmp}/zig.tar.xz"
  tar -xJf "${tmp}/zig.tar.xz" -C "${tmp}"
  # The archive expands to zig-${arch}-${os}-${ZIG_VERSION}/.
  extracted="${tmp}/zig-${arch}-${os}-${ZIG_VERSION}"
  if [ ! -d "${extracted}" ]; then
    # Fall back: take whatever single dir was extracted.
    extracted="$(find "${tmp}" -maxdepth 1 -type d -name 'zig-*' | head -1)"
  fi
  mv "${extracted}" "${zig_dir}"
  echo "zig ${ZIG_VERSION}: installed to ${zig_bin}"
fi

# ── tmux (warn only) ────────────────────────────────────────────────────
if ! command -v tmux >/dev/null 2>&1; then
  echo "warning: tmux not found — sessions won't persist across pilot restarts"
fi

# ── Print activation hint ───────────────────────────────────────────────
echo
echo "Bootstrap complete. To use pinned zig in this shell:"
echo "  export PATH=\"${zig_dir}:\$PATH\""
echo
echo "Or run via Makefile (which sets PATH automatically):"
echo "  make build"
