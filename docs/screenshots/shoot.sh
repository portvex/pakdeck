#!/usr/bin/env bash
# Runs inside a debian:stable-slim container (see scripts/screenshots.sh):
# drives pakdeck under tmux and saves each screen, with colours, as ANSI
# text in /out/<name>.ansi.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq tmux curl git htop jq tree >/dev/null
mkdir -p ~/.config/pakdeck
# tokyo-night sets its own background; ansi2svg.py uses the same colours for
# cells left at the terminal default, so change both together.
printf '[theme]\npreset = "tokyo-night"\n' > ~/.config/pakdeck/config.toml

W=${WIDTH:-120} H=${HEIGHT:-34}
cap() { tmux capture-pane -p -t t; }
shot() { sleep "${2:-1}"; tmux capture-pane -e -N -p -t t > "/out/$1.ansi"; echo "saved $1"; }
waitfor() { # seconds pattern
  for _ in $(seq 1 $(($1 * 4))); do cap | grep -q -- "$2" && return 0; sleep 0.25; done
  echo "timed out waiting for: $2"; cap; exit 1
}
keys() { tmux send-keys -t t "$@"; }

tmux new-session -d -s t -x "$W" -y "$H" -e COLORTERM=truecolor -e TERM=xterm-256color "pakdeck"
waitfor 20 "Installed"

# Installed tab: jump to apt with the filter, clear it again (the selection
# stays), so the full list shows with apt's details on the right (its
# maintainer is a team list, so no personal address is in the picture).
keys 2; sleep 1
keys / ; tmux send-keys -t t -l "apt"; keys Enter; sleep 1; keys Escape
shot installed 2

# Search the repositories.
keys 1; keys / ; tmux send-keys -t t -l "vim"; keys Enter
waitfor 30 "results"
shot search 3

# Theme picker: moving the selection previews the theme on the whole screen.
keys T; sleep 1; keys k k k k
shot themes 1
keys Escape; sleep 1

# Install a package through the embedded terminal pane, caught mid-run.
keys / BSpace BSpace BSpace; tmux send-keys -t t -l "fzf"; keys Enter
waitfor 30 "search 'fzf'"
keys L # an empty Log pane keeps the terminal pane's margin clean
keys i; waitfor 5 "Confirm"
keys y
for _ in $(seq 1 240); do
  if cap | grep -qi -e "\[y/n\]" -e "continue?"; then keys y Enter; fi
  cap | grep -q -e "Unpacking fzf" -e "Setting up fzf" && break
  sleep 0.25
done
shot terminal 0
waitfor 120 "Enter/Esc: close"
keys Enter
tmux kill-server
