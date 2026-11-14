#!/usr/bin/env bash
# Build .deb, .rpm, Arch (.pkg.tar.zst) and Alpine (.apk) packages from a
# static Linux binary with nfpm (packaging/nfpm.yaml).
#
# Usage: scripts/package.sh BINARY ARCH OUTDIR [VERSION]
#   ARCH is amd64 or arm64; VERSION defaults to the one in Cargo.toml.
#   scripts/package.sh target/x86_64-unknown-linux-musl/static/pakdeck amd64 dist
set -euo pipefail
if [[ $# -lt 3 ]]; then
  echo "usage: scripts/package.sh BINARY ARCH OUTDIR [VERSION]" >&2
  exit 2
fi
# Relative paths are relative to where the script is run from.
[[ -f "$1" ]] || { echo "package: no binary at $1" >&2; exit 1; }
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
arch=$2
mkdir -p "$3"
out=$(cd "$3" && pwd)
cd "$(dirname "$0")/.."
version=${4:-$(cargo metadata --no-deps --format-version 1 --locked |
  jq -r '.packages[] | select(.name == "pakdeck") | .version')}
case "$arch" in
  amd64 | arm64) ;;
  *) echo "package: ARCH must be amd64 or arm64, not $arch" >&2; exit 2 ;;
esac
command -v nfpm >/dev/null || { echo "package: nfpm not found (https://nfpm.goreleaser.com/install/)" >&2; exit 1; }
mkdir -p target/package
cp "$bin" target/package/pakdeck
for format in deb rpm archlinux apk; do
  VERSION=$version ARCH=$arch \
    nfpm package --config packaging/nfpm.yaml --packager "$format" --target "$out/"
done
ls -l "$out"
