#!/usr/bin/env bash
# Flatpak, Snap and several backends at once, in an Ubuntu VM (Lima):
# snapd needs systemd and flatpak needs user namespaces, which Docker
# cannot provide. Runs as Lima's normal user, so privileged commands go
# through sudo as on a desktop.
#
# Usage: tests/vm/ubuntu.sh            (creates the VM "pakdeck-ubuntu" if needed)
#        PAKDECK_VM_DELETE=1 tests/vm/ubuntu.sh   (delete the VM afterwards)
set -euo pipefail
cd "$(dirname "$0")/../.."
VM=pakdeck-ubuntu

if ! limactl list -q 2>/dev/null | grep -qx "$VM"; then
  echo "==> creating VM $VM"
  limactl start --name "$VM" --tty=false --cpus 4 --memory 4 template:ubuntu
elif [[ "$(limactl list --format '{{.Status}}' "$VM")" != Running ]]; then
  limactl start "$VM"
fi

echo "==> building static binary"
BIN=$(tests/build-linux.sh | tail -1)
# Lima mounts the home directory (read-only) at the same path in the VM.
limactl shell "$VM" -- sudo install -m 755 "$BIN" /usr/local/bin/pakdeck

echo "==> running checks"
rc=0
limactl shell "$VM" -- bash "$PWD/tests/vm/ubuntu-check.sh" || rc=$?
if [[ "${PAKDECK_VM_DELETE:-}" == 1 ]]; then
  limactl delete -f "$VM"
fi
[[ $rc == 0 ]] && echo "ubuntu VM: PASS" || echo "ubuntu VM: FAIL"
exit $rc
