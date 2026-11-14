#!/usr/bin/env bash
# Apply the repository rulesets in .github/rulesets/*.json to GitHub: each
# file creates the ruleset with its "name", or replaces the one that exists.
# Needs gh, authenticated as a repository admin.
#
# Usage: scripts/rulesets.sh [OWNER/REPO]   (default: the current repository)
set -euo pipefail
cd "$(dirname "$0")/.."
repo=${1:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}
existing=$(gh api "repos/$repo/rulesets" --paginate -q '.[] | "\(.id)\t\(.name)"')
for f in .github/rulesets/*.json; do
  name=$(jq -r .name "$f")
  id=$(awk -F'\t' -v n="$name" '$2 == n { print $1 }' <<<"$existing")
  if [[ -n "$id" ]]; then
    gh api -X PUT "repos/$repo/rulesets/$id" --input "$f" >/dev/null
    echo "rulesets: updated \"$name\""
  else
    gh api -X POST "repos/$repo/rulesets" --input "$f" >/dev/null
    echo "rulesets: created \"$name\""
  fi
done
