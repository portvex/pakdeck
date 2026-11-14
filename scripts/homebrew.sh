#!/usr/bin/env bash
# Write the Homebrew formula for a release (packaging/homebrew/pakdeck.rb.in
# with the version and the macOS archives' SHA-256s from SHA256SUMS).
#
# Usage: scripts/homebrew.sh TAG SHA256SUMS OUTFILE
#   scripts/homebrew.sh v0.1.1 dist/SHA256SUMS dist/pakdeck.rb
# Copy OUTFILE to Formula/pakdeck.rb in portvex/homebrew-tap.
set -euo pipefail
if [[ $# -ne 3 ]]; then
  echo "usage: scripts/homebrew.sh TAG SHA256SUMS OUTFILE" >&2
  exit 2
fi
tag=$1 sums=$2 out=$3
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "homebrew: TAG must look like v1.2.3 (no pre-releases), not $tag" >&2; exit 2; }
version=${tag#v}
template=$(dirname "$0")/../packaging/homebrew/pakdeck.rb.in

# The SHA-256 of one archive; exactly one line must name it.
sha() {
  local file="pakdeck-$tag-$1.tar.gz" lines
  lines=$(awk -v f="$file" '$2 == f || $2 == "*" f { print $1 }' "$sums")
  [[ $lines =~ ^[0-9a-f]{64}$ ]] || { echo "homebrew: no single SHA-256 for $file in $sums" >&2; return 1; }
  echo "$lines"
}
arm=$(sha aarch64-apple-darwin)
intel=$(sha x86_64-apple-darwin)
sed -e "s/@VERSION@/$version/g" -e "s/@SHA256_ARM@/$arm/" -e "s/@SHA256_INTEL@/$intel/" \
  "$template" > "$out"
if grep -q '@[A-Z0-9_]*@' "$out"; then
  echo "homebrew: unfilled placeholder in $out" >&2
  exit 1
fi
echo "wrote $out for $tag"
