#!/usr/bin/env bash
# Build the static Linux binary for the host architecture exactly as it is
# shipped (`cargo musl` / `cargo musl-arm`, .cargo/config.toml), inside a
# rust:alpine container so a musl host build stays covered.
# Output (last line): target/docker/<triple>/static/pakdeck
set -euo pipefail
cd "$(dirname "$0")/.."
case "$(uname -m)" in
  arm64 | aarch64) alias=musl-arm triple=aarch64-unknown-linux-musl ;;
  x86_64 | amd64) alias=musl triple=x86_64-unknown-linux-musl ;;
  *) echo "build-linux: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
docker run --rm -v "$PWD":/src -v pakdeck-cargo:/usr/local/cargo/registry \
  -w /src -e CARGO_TARGET_DIR=/src/target/docker rust:alpine \
  sh -c "apk add -q musl-dev >/dev/null && cargo -q $alias"
bin="target/docker/$triple/static/pakdeck"
# Smoke test: the binary must run in an image with nothing else in it.
img="pakdeck-scratch-smoke-$$"
ctx=$(mktemp -d)
trap 'docker rmi -f "$img" >/dev/null 2>&1 || true; rm -rf "$ctx"' EXIT
cp "$bin" "$ctx/pakdeck"
printf 'FROM scratch\nCOPY pakdeck /pakdeck\nENTRYPOINT ["/pakdeck"]\n' |
  docker build -q -t "$img" -f - "$ctx" >/dev/null
out=$(docker run --rm "$img" --help) || true
case "$out" in
  pakdeck:*) ;;
  *) echo "build-linux: $bin does not run without libc" >&2; exit 1 ;;
esac
echo "$PWD/$bin"
