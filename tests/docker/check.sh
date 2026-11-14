# Runs inside a distro container as root. Exercises every pakdeck operation for
# real, then drives the TUI under tmux, including an install through the
# embedded terminal pane.
set -euo pipefail
PKG=tree
# The binary is mounted at /usr/local/bin, which some images (nixos/nix) leave off PATH.
export PATH="/usr/local/bin:$PATH"
MIN_INSTALLED=5

fail() { echo "FAIL: $*"; exit 1; }
step() { echo "--- $*"; }

step "bootstrap (metadata + tmux for the TUI test)"
if command -v apt-get >/dev/null; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq && apt-get install -y -qq tmux >/dev/null
elif command -v dnf >/dev/null; then
  dnf install -y -q tmux >/dev/null
elif command -v zypper >/dev/null; then
  zypper -n -q refresh >/dev/null && zypper -n -q install tmux >/dev/null
elif command -v pacman >/dev/null; then
  # pacman's Landlock/alpm sandbox cannot be applied inside Docker.
  grep -q '^DisableSandbox' /etc/pacman.conf || sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf
  pacman -Sy --noconfirm --needed tmux >/dev/null
elif command -v apk >/dev/null; then
  apk add -q tmux >/dev/null
elif command -v xbps-install >/dev/null; then
  xbps-install -Syu xbps >/dev/null && xbps-install -Syu >/dev/null && xbps-install -Sy tmux >/dev/null
elif [[ "$(uname)" == FreeBSD ]]; then
  env ASSUME_ALWAYS_YES=yes pkg install -y tmux >/dev/null
elif command -v nix >/dev/null; then
  export NIX_CONFIG="experimental-features = nix-command flakes"
  nix profile install nixpkgs#tmux >/dev/null 2>&1
  MIN_INSTALLED=0
fi
export TERM=xterm-256color
# Some images (openSUSE) have no /run/tmux.
export TMUX_TMPDIR=/tmp

# Answers for interactive package-manager prompts (pipefail-safe, no `yes |`).
for _ in $(seq 50); do echo y; done > /tmp/yes

step "backend detection"
pakdeck -n --sync | tee /tmp/sync; [[ -s /tmp/sync ]] || fail "no sync command"

step "list installed"
n=$(pakdeck --list | wc -l); echo "$n installed packages"; (( n >= MIN_INSTALLED )) || fail "installed list too short"
pakdeck --list > /tmp/list; head -3 /tmp/list

step "explicit and orphans (must not error; unsupported is fine)"
pakdeck --explicit > /tmp/explicit 2>&1 || grep -q -e "cannot tell" -e "not available" /tmp/explicit || { cat /tmp/explicit; fail "explicit"; }
echo "explicit: $(wc -l < /tmp/explicit)"
pakdeck --orphans > /tmp/orphans 2>&1 || grep -qi "orphan" /tmp/orphans || { cat /tmp/orphans; fail "orphans"; }
echo "orphans: $(wc -l < /tmp/orphans)"

step "search"
pakdeck --search "$PKG" > /tmp/search; head -5 /tmp/search
grep -q "^$PKG[[:space:]]" /tmp/search || fail "search did not find $PKG"

step "info"
pakdeck --info "$PKG" > /tmp/info; head -8 /tmp/info; [[ -s /tmp/info ]] || fail "empty info"

step "updates (any count is fine; unsupported is fine)"
pakdeck --updates > /tmp/upd 2>&1 || grep -q "cannot list" /tmp/upd || { cat /tmp/upd; fail "updates"; }
head -5 /tmp/upd; echo "updates: $(wc -l < /tmp/upd)"

step "dry-run install prints commands and changes nothing"
pakdeck -n --install "$PKG"
pakdeck --list | grep -q "^$PKG[[:space:]]" && fail "$PKG installed before test"

step "install"
pakdeck --yes --install "$PKG" </tmp/yes >/tmp/out 2>&1 || { tail -20 /tmp/out; fail "install failed"; }
pakdeck --list | grep -q "^$PKG[[:space:]]" || fail "$PKG not in installed list after install"
command -v "$PKG" >/dev/null || fail "$PKG binary missing"

step "details of the installed package"
pakdeck --details "$PKG" > /tmp/details; cat /tmp/details | head -20
grep -q "^Files:" /tmp/details || fail "details missing"
# (no sed: the nixos/nix image does not have it)
grep -A100000 "^Files:" /tmp/details | grep -q "/bin/$PKG$" || fail "files of $PKG not listed"
grep -A100000 "^Files:" /tmp/details | grep -qE "^  /(usr|usr/bin)?$" && fail "directories listed as files"

step "dependencies are package names (tree nodes must be expandable)"
# bash: pacman lists "libreadline.so=8-64" next to "readline"; apk lists
# "/bin/sh" and "so:..." names; apt has PreDepends and virtual packages.
# Every entry must be a plain package name, and the list must not be empty.
# (Plain bash only: some images have no awk or sed.)
DEP_PKG=bash
if pakdeck --list | grep -q "^$DEP_PKG[[:space:]]"; then
  pakdeck --details "$DEP_PKG" > /tmp/deps 2>&1 || { cat /tmp/deps; fail "details of $DEP_PKG"; }
  : > /tmp/deplist
  on=0
  while IFS= read -r line; do
    case "$line" in
      "Depends on:") on=1 ;;
      [A-Z]*) on=0 ;;
      *) [[ $on == 1 ]] && echo "$line" >> /tmp/deplist ;;
    esac
  done < /tmp/deps
  cat /tmp/deplist
  [[ -s /tmp/deplist ]] || fail "no dependencies listed for $DEP_PKG"
  while IFS= read -r line; do
    [[ "$line" =~ ^\ \ [A-Za-z0-9][A-Za-z0-9+._@-]*$ ]] || fail "not a package name in the dependency list: '$line'"
    [[ "$line" == *.so* ]] && fail "soname in the dependency list: '$line'"
  done < /tmp/deplist
  want=""
  if command -v pacman >/dev/null; then want="readline"
  elif command -v apk >/dev/null; then want="musl readline"
  elif command -v apt-get >/dev/null; then want="libc6"
  elif command -v xbps-install >/dev/null || command -v dnf >/dev/null || command -v zypper >/dev/null; then want="glibc"
  fi
  for w in $want; do
    [[ $(grep -cx "  $w" /tmp/deplist) == 1 ]] || fail "expected $w exactly once in $DEP_PKG's dependencies"
  done
else
  echo "($DEP_PKG is not installed here; skipped)"
fi

step "versions and install-version (unsupported is fine)"
if pakdeck --versions "$PKG" > /tmp/versions 2>&1; then
  head -5 /tmp/versions
  V=$(head -1 /tmp/versions | cut -f1)   # the id (snap prints "channel<TAB>version")
  [[ -n "$V" ]] || fail "empty version list"
  pakdeck -n --install-version "$PKG" "$V" | tee /tmp/iv
  [[ -s /tmp/iv ]] || fail "no install-version commands"
else
  grep -q -e "cannot list versions" -e "not available" /tmp/versions || { cat /tmp/versions; fail "versions"; }
  echo "versions not available: $(tail -1 /tmp/versions)"
fi

step "hold and release (pacman: IgnorePkg in pacman.conf; nix: none)"
if pakdeck --yes --hold "$PKG" >/tmp/out 2>&1; then
  pakdeck --held | tee /tmp/held
  grep -q "	$PKG\$" /tmp/held || fail "$PKG not in the held list"
  pakdeck --yes --unhold "$PKG" >/tmp/out 2>&1 || { tail -10 /tmp/out; fail "unhold"; }
  pakdeck --held | grep -q "	$PKG\$" && fail "$PKG still held after unhold"
else
  grep -q -e "cannot hold" -e "IgnorePkg" /tmp/out || { tail -10 /tmp/out; fail "hold"; }
  echo "holds not available: $(tail -1 /tmp/out)"
fi

step "export"
pakdeck --export /tmp/export.toml
cat /tmp/export.toml | head -20
grep -q "\"$PKG\"" /tmp/export.toml || fail "$PKG not exported as explicitly installed"

step "upgrade (selected package)"
pakdeck --yes --upgrade "$PKG" </tmp/yes >/tmp/out 2>&1 || { tail -20 /tmp/out; fail "upgrade failed"; }

step "remove"
pakdeck --yes --remove "$PKG" </tmp/yes >/tmp/out 2>&1 || { tail -20 /tmp/out; fail "remove failed"; }
pakdeck --list | grep -q "^$PKG[[:space:]]" && fail "$PKG still installed after remove"

step "history records the runs; undo of the removal plans the install"
pakdeck --history > /tmp/history; head -5 /tmp/history
grep -q "^1[[:space:]].*[[:space:]]ok[[:space:]]remove $PKG$" /tmp/history || fail "removal not recorded"
pakdeck -n --undo > /tmp/undo; cat /tmp/undo
grep -q "$PKG" /tmp/undo || fail "undo plan does not reinstall $PKG"

step "import plan reinstalls what the export lists"
pakdeck -n --import /tmp/export.toml > /tmp/plan 2>&1; cat /tmp/plan
# The install verb differs (apk add, pacman -S, ...); the package is the last word
# (nix installs a flake reference: 'nixpkgs#tree')
grep -qE "[[:space:]'#]$PKG'?\$" /tmp/plan || fail "import plan does not install $PKG"

step "cleanup"
pakdeck -n --cleanup
pakdeck --yes --cleanup </tmp/yes >/tmp/out 2>&1 || { tail -20 /tmp/out; fail "cleanup failed"; }

step "TUI: search, dry-run, install through the embedded terminal pane"
cap() { tmux capture-pane -p -t t; }
# waitfor SECONDS PATTERN... (any pattern; plain `grep -e`, which BSD grep
# supports too, unlike the GNU-only `\|`)
waitfor() {
  local secs=$1; shift
  local args=(); for p in "$@"; do args+=(-e "$p"); done
  for _ in $(seq $(( secs * 2 ))); do cap | grep -q "${args[@]}" && return 0; sleep 0.5; done
  cap; fail "timed out waiting for: $*"
}
tmux new-session -d -s t -x 140 -y 40 "pakdeck"
waitfor 20 "Installed ("
tmux send-keys -t t / ; tmux send-keys -t t -l "$PKG"; tmux send-keys -t t Enter
waitfor 180 "search '$PKG'"
waitfor 10 "> \[ \]   $PKG"
tmux send-keys -t t D; tmux send-keys -t t i; waitfor 5 "Confirm"
tmux send-keys -t t y; waitfor 5 "\[dry-run\] \$ .*$PKG"
tmux send-keys -t t D; tmux send-keys -t t i; waitfor 5 "Confirm"
tmux send-keys -t t y
waitfor 30 "Running: Install $PKG" "Finished: Install $PKG"
# Answer any y/n prompt through the pseudo-terminal.
for _ in $(seq 120); do
  cap | grep -q -e "Finished: Install" -e "Failed: Install" && break
  if cap | grep -qi -e "\[y/n\]" -e "continue?"; then tmux send-keys -t t y Enter; fi
  sleep 1
done
echo "terminal pane:"; cap | head -40
cap | grep -q "Finished: Install $PKG" || fail "install through the terminal pane did not finish"
tmux send-keys -t t Enter; waitfor 10 "done"
pakdeck --list | grep -q "^$PKG[[:space:]]" || fail "$PKG not installed after TUI install"

step "TUI: orphans tab, explicit filter, details sections"
tmux send-keys -t t 3; waitfor 5 "Orphans"
tmux send-keys -t t 2; tmux send-keys -t t e; sleep 2
cap | grep -q -e "(explicit)" -e "cannot tell" || { cap; fail "explicit toggle"; }
tmux send-keys -t t e; sleep 0.5
tmux send-keys -t t / ; tmux send-keys -t t -l "$PKG"; tmux send-keys -t t Escape
waitfor 20 "Depends on" "Required by"
echo "TUI screen:"; cap | head -40
cap | grep -q "error loading" && fail "a list failed to load (see the log pane above)"
tmux send-keys -t t q; sleep 0.5
tmux has-session -t t 2>/dev/null && fail "TUI did not quit"

if command -v pacman >/dev/null; then
  step "AUR (opt-in): build yay-bin as a user, then search, install, list, remove through pakdeck"
  pacman -S --noconfirm --needed base-devel git sudo >/dev/null
  id builder >/dev/null 2>&1 || useradd -m builder
  echo "builder ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/builder
  # makepkg refuses to run as root; yay and paru do too.
  su - builder -c "rm -rf yay-bin && git clone -q https://aur.archlinux.org/yay-bin.git && cd yay-bin && makepkg -si --noconfirm --needed" >/tmp/yay.log 2>&1 \
    || { tail -20 /tmp/yay.log; fail "building yay-bin"; }
  AUR_PKG=pfetch
  as_user() { su - builder -c "PATH=/usr/local/bin:\$PATH $*"; }
  as_user "pakdeck -b pacman,aur -n --sync" | tee /tmp/aursync
  as_user "pakdeck -b pacman,aur --search $AUR_PKG" > /tmp/aursearch; head -3 /tmp/aursearch
  grep -q "^$AUR_PKG[[:space:]].*[[:space:]]aur$" /tmp/aursearch || fail "AUR search did not find $AUR_PKG"
  as_user "pakdeck -b pacman,aur --yes --install aur:$AUR_PKG" > /tmp/aurout 2>&1 || { tail -20 /tmp/aurout; fail "AUR install"; }
  as_user "pakdeck -b pacman,aur --list" > /tmp/aurlist
  grep -q "^$AUR_PKG[[:space:]].*[[:space:]]aur$" /tmp/aurlist || { tail -15 /tmp/aurout; grep "$AUR_PKG" /tmp/aurlist; pacman -Qm; fail "$AUR_PKG not listed from aur"; }
  grep -q "^$AUR_PKG[[:space:]].*[[:space:]]pacman$" /tmp/aurlist && fail "$AUR_PKG listed twice (pacman duplicate)"
  as_user "pakdeck -b pacman,aur --updates" >/dev/null || fail "AUR updates"
  as_user "pakdeck -b pacman,aur --details aur:$AUR_PKG" > /tmp/aurdetails; head -5 /tmp/aurdetails
  grep -q "/usr/bin/$AUR_PKG$" /tmp/aurdetails || fail "details of the installed AUR package have no files"
  as_user "pakdeck -b pacman,aur --yes --remove aur:$AUR_PKG" > /tmp/aurout 2>&1 || { tail -20 /tmp/aurout; fail "AUR remove"; }
  as_user "pakdeck -b pacman,aur --list" | grep -q "^$AUR_PKG[[:space:]]" && fail "$AUR_PKG still installed"
fi

echo "ALL CHECKS PASSED"
