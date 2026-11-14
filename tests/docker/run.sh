#!/usr/bin/env bash
# Integration tests for the Linux backends. Builds a static musl binary in a
# rust:alpine container, then runs tests/docker/check.sh inside each distro.
#
# Usage: tests/docker/run.sh [debian|ubuntu|fedora|opensuse|arch|alpine|void|nix ...]
# PAKDECK_BIN=/abs/path/pakdeck skips the build and tests that binary (CI).
set -euo pipefail
cd "$(dirname "$0")/../.."

declare -A IMAGES=(
  [debian]=debian:stable-slim
  [ubuntu]=ubuntu:24.04
  [fedora]=fedora:latest
  [arch]=menci/archlinuxarm:latest   # official archlinux image is amd64-only
  [alpine]=alpine:latest
  [opensuse]=opensuse/tumbleweed:latest
  [void]=ghcr.io/void-linux/void-glibc-full:latest
  [nix]=nixos/nix:latest
)
if [[ "$(uname -m)" == x86_64 ]]; then IMAGES[arch]=archlinux:latest; fi
DISTROS=("$@")
[[ ${#DISTROS[@]} -eq 0 ]] && DISTROS=(debian ubuntu fedora opensuse arch alpine void nix)

if [[ -n "${PAKDECK_BIN:-}" ]]; then
  BIN=$PAKDECK_BIN
  echo "==> using $BIN"
else
  echo "==> building static binary"
  BIN=$(tests/build-linux.sh | tail -1)
fi

declare -A RESULT
for d in "${DISTROS[@]}"; do
  echo; echo "==================== $d (${IMAGES[$d]}) ===================="
  if docker run --rm -e TERM=xterm-256color \
      -v "$BIN":/usr/local/bin/pakdeck:ro \
      -v "$PWD/tests/docker/check.sh":/check.sh:ro \
      "${IMAGES[$d]}" sh -c "command -v bash >/dev/null || apk add -q bash 2>/dev/null || { xbps-install -Syu xbps && xbps-install -Sy bash; } >/dev/null; exec bash /check.sh" 2>&1 | sed "s/^/[$d] /"; then
    RESULT[$d]=PASS
  else
    RESULT[$d]=FAIL
  fi
done

echo; echo "==================== summary ===================="
rc=0
for d in "${DISTROS[@]}"; do echo "$d: ${RESULT[$d]}"; [[ ${RESULT[$d]} == PASS ]] || rc=1; done
exit $rc
