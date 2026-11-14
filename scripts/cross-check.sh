#!/usr/bin/env bash
# Run clippy (with the no-panic lints) for platforms this machine does not
# run: Linux, FreeBSD and Windows code paths are behind cfg() gates.
# Targets missing from rustup are skipped with a hint, or are an error when
# CROSS_CHECK_STRICT=1 (CI).
set -euo pipefail
cd "$(dirname "$0")/.."
command -v rustup >/dev/null || { echo "cross-check: rustup not found, skipped"; exit 0; }
# rustup's proxies must win over a Homebrew cargo/clippy earlier in PATH.
export PATH="$HOME/.cargo/bin:$PATH"
installed=$(rustup target list --installed --toolchain stable)
for t in x86_64-unknown-linux-gnu x86_64-unknown-freebsd x86_64-pc-windows-msvc; do
  if grep -qx "$t" <<<"$installed"; then
    echo "cross-check: $t"
    cargo +stable clippy --quiet --locked --target "$t" --all-targets -- -D warnings
  else
    echo "cross-check: $t skipped (rustup target add $t --toolchain stable)"
    if [[ "${CROSS_CHECK_STRICT:-0}" == 1 ]]; then
      echo "cross-check: missing target with CROSS_CHECK_STRICT=1" >&2
      exit 1
    fi
  fi
done
