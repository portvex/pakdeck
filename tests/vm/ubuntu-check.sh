# Runs inside the Ubuntu VM as the (sudo-capable) Lima user.
set -euo pipefail
SNAP=hello
FLAT=io.sourceforge.pipewalker.PipeWalker

fail() { echo "FAIL: $*"; exit 1; }
step() { echo "--- $*"; }
has_row() { grep -qP "^$1\t.*\t$2\$" ; }  # name ... source (last column)

step "bootstrap: flatpak + flathub, tmux"
# Release holds a failed earlier run may have left, so the slate is clean.
sudo apt-mark unhold tree >/dev/null 2>&1 || true
sudo snap refresh --unhold "$SNAP" >/dev/null 2>&1 || true
sudo flatpak mask --remove "$FLAT" >/dev/null 2>&1 || true
sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq flatpak tmux tree- >/dev/null
sudo flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
sudo flatpak update --appstream -y >/dev/null
# Start from a clean slate if a previous run left things behind.
snap list "$SNAP" >/dev/null 2>&1 && sudo snap remove "$SNAP" >/dev/null
flatpak info "$FLAT" >/dev/null 2>&1 && sudo flatpak uninstall -y "$FLAT" >/dev/null
export TERM=xterm-256color
mkdir -p ~/.config/pakdeck
cat > ~/.config/pakdeck/config.toml <<'CFG'
backends = ["apt", "flatpak", "snap"]
export_path = "/tmp/pakdeck-export.toml"
CFG

step "default backend without config is apt only"
pakdeck -c /dev/null -n --sync | grep -q "apt-get update" || fail "default backend"
pakdeck -c /dev/null -n --sync | grep -q flatpak && fail "flatpak active without config"

step "several backends: sync runs all of them"
pakdeck -n --sync | tee /tmp/sync
grep -q "apt-get update" /tmp/sync && grep -q "flatpak update --appstream" /tmp/sync || fail "multi sync"

step "search across managers"
pakdeck --search pipewalker > /tmp/s1; head -3 /tmp/s1
has_row "$FLAT" flatpak < /tmp/s1 || fail "flatpak search"
pakdeck --search "$SNAP" > /tmp/s2; head -3 /tmp/s2
has_row "$SNAP" snap < /tmp/s2 || fail "snap search"

step "install a snap and a flatpak (source:name), with --yes"
pakdeck --yes --install "snap:$SNAP" > /tmp/out 2>&1 || { tail -20 /tmp/out; fail "snap install"; }
grep -q "sudo snap install" /tmp/out || fail "snap install not run through sudo"
pakdeck --yes --install "flatpak:$FLAT" > /tmp/out 2>&1 || { tail -20 /tmp/out; fail "flatpak install"; }
pakdeck --list > /tmp/list
has_row "$SNAP" snap < /tmp/list || fail "snap not listed as installed"
has_row "$FLAT" flatpak < /tmp/list || fail "flatpak not listed as installed"
# --explicit prints source<TAB>name
pakdeck --explicit | grep -qxP "flatpak\t$FLAT" || fail "flatpak app not explicit"
pakdeck --explicit | grep -qxP "snap\t$SNAP" || fail "snap not explicit"

step "details"
pakdeck --details "flatpak:$FLAT" | tee /tmp/d | head -8
grep -A1 "^Depends on:" /tmp/d | grep -q "org.freedesktop.Platform" || fail "flatpak runtime dependency"
pakdeck --details "snap:$SNAP" | grep -A2 "^Files:" | grep -q "/snap/$SNAP/" || fail "snap files"

step "hold and release: snap, flatpak, apt"
pakdeck --yes --install tree > /tmp/out 2>&1 || { tail -5 /tmp/out; fail "apt install"; }
for t in "snap:$SNAP" "flatpak:$FLAT" "apt:tree"; do
  pakdeck --yes --hold "$t" > /tmp/out 2>&1 || { tail -5 /tmp/out; fail "hold $t"; }
done
pakdeck --held | tee /tmp/held
for row in "snap	$SNAP" "flatpak	$FLAT" "apt	tree"; do grep -qx "$row" /tmp/held || fail "not held: $row"; done
apt-mark showhold | grep -qx tree || fail "apt-mark does not show the hold"

step "export"
pakdeck --export
cat /tmp/pakdeck-export.toml
section() { sed -n "/^\[$1\]/,/^\[/p" /tmp/pakdeck-export.toml; }
section snap | grep -q "\"$SNAP\"" || fail "snap not exported"
section flatpak | grep -q "$FLAT" || fail "flatpak not exported"
section apt | grep -q 'held = \["tree"\]' || fail "apt hold not exported"

step "release, remove, then import restores packages and holds"
for t in "snap:$SNAP" "flatpak:$FLAT" "apt:tree"; do pakdeck --yes --unhold "$t" >/dev/null 2>&1 || fail "unhold $t"; done
[[ -z "$(pakdeck --held)" ]] || fail "holds remain after unhold"
pakdeck --yes --remove "snap:$SNAP" "flatpak:$FLAT" > /tmp/out 2>&1 || { tail -10 /tmp/out; fail "remove"; }
pakdeck --list | grep -qP "^$SNAP\t" && fail "snap still installed"
pakdeck -n --import /tmp/pakdeck-export.toml | tee /tmp/plan
# The export keeps the flatpak remote, so the plan installs from flathub.
grep -q "snap install $SNAP" /tmp/plan && grep -q "flatpak install flathub $FLAT" /tmp/plan || fail "import plan"
pakdeck --yes --import /tmp/pakdeck-export.toml > /tmp/out 2>&1 || { tail -20 /tmp/out; fail "import"; }
pakdeck --list > /tmp/list
has_row "$SNAP" snap < /tmp/list && has_row "$FLAT" flatpak < /tmp/list || fail "import did not reinstall"
pakdeck --held | grep -qx "apt	tree" || fail "import did not restore the apt hold"

step "TUI: several sources, source filter, hold marker, hold through the terminal pane"
export TMUX_TMPDIR=/tmp
cap() { tmux capture-pane -p -t t; }
waitfor() { for _ in $(seq $(( $2 * 2 ))); do cap | grep -q "$1" && return 0; sleep 0.5; done; cap; fail "timed out waiting for: $1"; }
# A failed earlier run may have left its session; always clean up ours.
tmux kill-session -t t 2>/dev/null || true
trap 'tmux kill-session -t t 2>/dev/null || true' EXIT
tmux new-session -d -s t -x 160 -y 45 "pakdeck"
waitfor "apt+flatpak+snap" 20
waitfor "Installed (" 30
tmux send-keys -t t 2; tmux send-keys -t t / ; tmux send-keys -t t -l tree; tmux send-keys -t t Escape
waitfor "H \[apt\] tree" 20
tmux send-keys -t t Escape; tmux send-keys -t t f; waitfor "\[apt only\]" 5
tmux send-keys -t t f; tmux send-keys -t t f; waitfor "\[snap only\]" 5
tmux send-keys -t t / ; tmux send-keys -t t -l "$SNAP"; tmux send-keys -t t Escape
# The import restored the hold on $SNAP: p releases it, p again holds it.
waitfor "> \[ \] H \[snap\] $SNAP " 10
tmux send-keys -t t p; waitfor "Release $SNAP" 5
tmux send-keys -t t y; waitfor "Finished: Release $SNAP" 60
tmux send-keys -t t Enter; waitfor "done" 10
waitfor "> \[ \]   \[snap\] $SNAP " 20
tmux send-keys -t t p; waitfor "Hold $SNAP" 5
tmux send-keys -t t y; waitfor "Finished: Hold $SNAP" 60
tmux send-keys -t t Enter
waitfor "> \[ \] H \[snap\] $SNAP " 20
echo "TUI screen:"; cap | head -45
cap | grep -q "error loading" && fail "a list failed to load"
tmux send-keys -t t q; sleep 0.5

step "clean up"
pakdeck --yes --unhold "snap:$SNAP" "apt:tree" >/dev/null 2>&1 || true
pakdeck --yes --remove "snap:$SNAP" "flatpak:$FLAT" >/dev/null 2>&1 || true
pakdeck --yes --remove tree >/dev/null 2>&1 || true
rm -f ~/.config/pakdeck/config.toml

echo "ALL CHECKS PASSED"
