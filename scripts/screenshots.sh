#!/usr/bin/env bash
# Regenerate the README screenshots (docs/screenshots/*.svg): runs pakdeck in
# a clean Debian container (so the shots show stock packages, not this
# machine's), drives it under tmux (docs/screenshots/shoot.sh), and turns the
# captures into SVG with rich (docs/screenshots/ansi2svg.py, run through uv).
#
# Usage: scripts/screenshots.sh   (needs Docker, uv, and `cargo musl` or
#                                  `cargo musl-arm` built for this machine)
set -euo pipefail
cd "$(dirname "$0")/.."
case "$(uname -m)" in
  arm64 | aarch64) bin=target/aarch64-unknown-linux-musl/static/pakdeck ;;
  *) bin=target/x86_64-unknown-linux-musl/static/pakdeck ;;
esac
[[ -x "$bin" ]] || { echo "screenshots: build $bin first (cargo musl / cargo musl-arm)" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
docker run --rm \
  -v "$PWD/$bin":/usr/local/bin/pakdeck:ro \
  -v "$PWD/docs/screenshots/shoot.sh":/shoot.sh:ro \
  -v "$work":/out debian:stable-slim bash /shoot.sh
uv run --quiet --with 'rich==14.*' python docs/screenshots/ansi2svg.py "$work" docs/screenshots
