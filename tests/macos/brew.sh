#!/usr/bin/env bash
# Real-terminal tests of the Homebrew backend against a throwaway Homebrew
# in a temporary directory, so the user's own Homebrew is never touched.
#
# Homebrew only ships bottles for its default prefix, so packages build from
# source here; the test uses `tree`, which builds in seconds with no deps.
# Needs the Xcode command line tools (git, a compiler) and tmux.
#
# Usage: tests/macos/brew.sh        (PAKDECK_KEEP=1 keeps the temporary directory)
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$PWD
PKG=tree

fail() { echo "FAIL: $*"; exit 1; }
step() { echo "--- $*"; }

[[ "$(uname -s)" == Darwin ]] || { echo "macOS only"; exit 1; }
command -v tmux >/dev/null || { echo "tmux is required"; exit 1; }

echo "==> building pakdeck"
cargo build --release --locked -q
PAKDECK_BIN=$ROOT/target/release/pakdeck

# Canonical path (/var is a symlink to /private/var), so the safety check
# below compares like with like.
WORK=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/pakdeck-brew-test.XXXXXX")" && pwd -P)
cleanup() {
  tmux -L pakdeck-test kill-server 2>/dev/null || true
  [[ "${PAKDECK_KEEP:-}" == 1 ]] && { echo "kept $WORK"; return; }
  chmod -R u+w "$WORK" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

PREFIX=$WORK/homebrew
mkdir -p "$WORK/bin" "$WORK/config"
echo "==> installing a throwaway Homebrew in $PREFIX"
git clone -q --depth=1 https://github.com/Homebrew/brew "$PREFIX"

# Everything Homebrew writes stays under $WORK.
export HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1
export HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_INSTALL_UPGRADE=1
export HOMEBREW_CACHE=$WORK/cache HOMEBREW_LOGS=$WORK/logs HOMEBREW_TEMP=$WORK/tmp
mkdir -p "$HOMEBREW_TEMP"
ln -s "$PAKDECK_BIN" "$WORK/bin/pakdeck"
export PATH="$WORK/bin:$PREFIX/bin:$PATH"
# An empty pakdeck config: no user settings affect the test.
export XDG_CONFIG_HOME=$WORK/config
# The history goes here too, never into the user's own.
export XDG_STATE_HOME=$WORK/state
export TERM=xterm-256color

# Safety: every brew call below must reach the throwaway Homebrew.
[[ "$(command -v brew)" == "$PREFIX/bin/brew" ]] || fail "brew resolves to $(command -v brew), not the throwaway one"
[[ "$(brew --prefix)" == "$PREFIX" ]] || fail "brew --prefix is $(brew --prefix)"
brew --version | head -1

step "backend detection and sync"
pakdeck -n --sync | tee "$WORK/sync"
grep -q "^brew update" "$WORK/sync" || fail "brew not detected"

step "search"
pakdeck --search "$PKG" > "$WORK/search"; head -3 "$WORK/search"
grep -q "^$PKG	" "$WORK/search" || fail "search did not find $PKG"

step "install (builds from source in a non-default prefix)"
pakdeck --yes --install "$PKG" > "$WORK/out" 2>&1 || { tail -20 "$WORK/out"; fail "install"; }
[[ -x "$PREFIX/bin/$PKG" ]] || fail "$PKG not installed into the throwaway prefix"
pakdeck --list | grep -q "^$PKG	" || fail "$PKG not listed"
pakdeck --explicit | grep -q "brew	$PKG" || fail "$PKG not explicit"

step "updates (greedy listing; HOMEBREW_NO_AUTO_UPDATE is respected)"
pakdeck --updates > "$WORK/updates" 2>&1 || { cat "$WORK/updates"; fail "updates"; }
pakdeck -n --upgrade | grep -q "^brew upgrade --greedy$" || fail "update-all is not greedy"

step "details and versions"
pakdeck --details "$PKG" > "$WORK/details"; head -8 "$WORK/details"
grep -A50 "^Files:" "$WORK/details" | grep -q "/bin/$PKG$" || fail "files"
if pakdeck --versions "$PKG" > "$WORK/versions" 2>&1; then
  fail "brew unexpectedly listed versions: $(cat "$WORK/versions")"
fi
grep -q "cannot list versions" "$WORK/versions" || fail "versions message: $(cat "$WORK/versions")"

step "hold (brew pin) and release"
pakdeck --yes --hold "$PKG" > "$WORK/out" 2>&1 || { cat "$WORK/out"; fail "hold"; }
pakdeck --held | grep -qx "brew	$PKG" || fail "not held"
pakdeck --yes --unhold "$PKG" > "$WORK/out" 2>&1 || { cat "$WORK/out"; fail "unhold"; }
pakdeck --held | grep -q "$PKG" && fail "still held"

step "export and import plan"
pakdeck --export - > "$WORK/export.toml" 2>/dev/null
grep -q "\"$PKG\"" "$WORK/export.toml" || fail "not exported"

step "TUI: search highlight, sort, theme, versions message, dry-run install"
T=(tmux -L pakdeck-test)
cap() { "${T[@]}" capture-pane -p -t t; }
waitfor() { local s=$1; shift; local a=(); for p in "$@"; do a+=(-e "$p"); done
  for _ in $(seq $(( s * 2 ))); do cap | grep -q "${a[@]}" && return 0; sleep 0.5; done
  cap; fail "timed out waiting for: $*"; }
"${T[@]}" new-session -d -s t -x 140 -y 40 "pakdeck"
waitfor 30 "Installed ("
"${T[@]}" send-keys -t t 2; waitfor 10 "$PKG"
"${T[@]}" send-keys -t t o; waitfor 5 "(by name)"
"${T[@]}" send-keys -t t o; waitfor 5 "(by install date)" "(by size)"
"${T[@]}" send-keys -t t T; waitfor 5 "Themes"
# Preview the next theme, then go back to the old one (nothing is saved).
"${T[@]}" send-keys -t t j; sleep 0.3
"${T[@]}" send-keys -t t Escape; sleep 0.5
[[ -e "$XDG_CONFIG_HOME/pakdeck/config.toml" ]] && fail "Esc in the theme picker wrote the config"
"${T[@]}" send-keys -t t v; waitfor 10 "cannot list versions"
"${T[@]}" send-keys -t t 1 / ; "${T[@]}" send-keys -t t -l "$PKG"; "${T[@]}" send-keys -t t Enter
waitfor 60 "search '$PKG'"
"${T[@]}" send-keys -t t D i; waitfor 5 "Confirm"
"${T[@]}" send-keys -t t y; waitfor 5 "\[dry-run\] \$ brew install"
"${T[@]}" send-keys -t t 2 z; waitfor 20 "Largest packages"
# Separate sends: Escape immediately followed by R reads as Alt+R.
"${T[@]}" send-keys -t t Escape; sleep 0.5
"${T[@]}" send-keys -t t R; waitfor 20 "Taps"
"${T[@]}" send-keys -t t Escape
echo "TUI screen:"; cap | head -40
cap | grep -q "error loading" && fail "a list failed to load"
"${T[@]}" send-keys -t t q; sleep 0.5

step "taps: a local tap (no network), list, filter, untap"
brew tap-new --no-git pakdeck-test/local > "$WORK/out" 2>&1 || { cat "$WORK/out"; fail "tap-new"; }
pakdeck --repos | tee "$WORK/repos"
grep -q "^brew	pakdeck-test/local	" "$WORK/repos" || fail "tap not listed"
pakdeck -n --add-repo pakdeck-test/other | grep -q "brew tap pakdeck-test/other" || fail "add-repo plan"
{ pakdeck -n --add-repo justname 2>&1 || true; } | grep -q "user/repo" || fail "bad tap name accepted"
pakdeck --yes --remove-repo pakdeck-test/local > "$WORK/out" 2>&1 || { cat "$WORK/out"; fail "untap"; }
pakdeck --repos | grep -q "pakdeck-test/local" && fail "still tapped"

step "disk usage"
pakdeck --disk | tee "$WORK/disk"
grep -q "^total	brew	.*	1/1$" "$WORK/disk" || fail "disk totals"
grep -q "^package	$PKG	" "$WORK/disk" || fail "$PKG not among the largest"

step "remove"
pakdeck --yes --remove "$PKG" > "$WORK/out" 2>&1 || { tail -10 "$WORK/out"; fail "remove"; }
pakdeck --list | grep -q "^$PKG	" && fail "still installed"

step "history and undo (undo of the removal reinstalls)"
pakdeck --history | tee "$WORK/history"
grep -q "^1	.*	ok	remove $PKG$" "$WORK/history" || fail "removal not recorded"
grep -q "	ok	install $PKG$" "$WORK/history" || fail "install not recorded"
[[ -s "$WORK/state/pakdeck/history.jsonl" ]] || fail "history not in XDG_STATE_HOME"
pakdeck -n --undo | grep -q "^brew install $PKG$" || fail "undo plan"
pakdeck --yes --undo > "$WORK/out" 2>&1 || { tail -10 "$WORK/out"; fail "undo"; }
pakdeck --list | grep -q "^$PKG	" || fail "undo did not reinstall $PKG"
pakdeck --history | head -1 | grep -q "Undo: remove $PKG" || fail "undo not recorded"
pakdeck --yes --remove "$PKG" > "$WORK/out" 2>&1 || fail "remove again"

echo "ALL CHECKS PASSED"
