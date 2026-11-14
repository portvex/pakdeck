#!/usr/bin/env bash
# Build and test pakdeck on FreeBSD in a qemu VM (arm64, hardware-accelerated
# with HVF on Apple Silicon, KVM on Linux arm64 hosts).
#
# First run (slow, once): boot the official BASIC-CLOUDINIT image, let its
# first-boot base-system update finish, install Rust, bash and tmux from pkg,
# and save the result as a provisioned image in tests/vm/.cache. A throwaway
# SSH key is installed for root through a cloud-init (nuageinit) seed ISO.
#
# Every run: boot a copy-on-write overlay of the provisioned image (so runs
# start clean), copy the source in, build pakdeck and run its unit tests
# natively, then run tests/docker/check.sh against the pkg backend for real,
# including the TUI and its embedded terminal pane.
#
# Delete tests/vm/.cache/freebsd-*-provisioned.qcow2 to provision again.
#
# Usage: tests/vm/freebsd.sh              (PAKDECK_VM_KEEP=1 leaves the VM running)
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$PWD
CACHE=$ROOT/tests/vm/.cache
RUN=$CACHE/freebsd-run
VER=15.1
IMG=FreeBSD-$VER-RELEASE-arm64-aarch64-BASIC-CLOUDINIT-ufs.qcow2
BASE=https://download.freebsd.org/releases/VM-IMAGES/$VER-RELEASE/aarch64/Latest
MIRROR=${PAKDECK_FREEBSD_MIRROR:-$BASE}
PORT=${PAKDECK_VM_SSH_PORT:-2222}
mkdir -p "$CACHE" "$RUN"

# --- image (verified against the official checksum file)
if [[ ! -f "$CACHE/$IMG" ]]; then
  echo "==> downloading $IMG.xz"
  curl -sSfL -o "$CACHE/CHECKSUM.SHA256" "$BASE/CHECKSUM.SHA256"
  curl -sSfL -C - -o "$CACHE/$IMG.xz" "$MIRROR/$IMG.xz"
  want=$(grep "($IMG.xz)" "$CACHE/CHECKSUM.SHA256" | awk '{print $NF}')
  got=$(shasum -a 256 "$CACHE/$IMG.xz" | awk '{print $1}')
  [[ -n "$want" && "$want" == "$got" ]] || { echo "checksum mismatch for $IMG.xz"; exit 1; }
  xz -dk "$CACHE/$IMG.xz"
fi

# --- firmware and accelerator
FW=""
for d in /opt/homebrew/share/qemu /usr/local/share/qemu /usr/share/qemu /usr/share/AAVMF; do
  [[ -f $d/edk2-aarch64-code.fd ]] && FW=$d/edk2-aarch64-code.fd && break
  [[ -f $d/AAVMF_CODE.fd ]] && FW=$d/AAVMF_CODE.fd && break
done
[[ -n "$FW" ]] || { echo "no aarch64 UEFI firmware found (edk2-aarch64-code.fd)"; exit 1; }
case "$(uname -s)" in
  Darwin) ACCEL=(-accel hvf -cpu host) ;;
  Linux) if [[ -w /dev/kvm && "$(uname -m)" == aarch64 ]]; then ACCEL=(-accel kvm -cpu host); else ACCEL=(-cpu max); fi ;;
  *) ACCEL=(-cpu max) ;;
esac

# --- key and cloud-init seed
rm -f "$RUN"/seed.iso
[[ -f "$CACHE/id_ed25519" ]] || ssh-keygen -q -t ed25519 -N "" -C pakdeck-test -f "$CACHE/id_ed25519"
KEY=$(cat "$CACHE/id_ed25519.pub")
SEED=$(mktemp -d)
cat > "$SEED/meta-data" <<META
instance-id: pakdeck-test
local-hostname: pakdeck-freebsd
META
cat > "$SEED/user-data" <<USER
#cloud-config
ssh_authorized_keys:
  - $KEY
runcmd:
  - mkdir -p /root/.ssh && chmod 700 /root/.ssh
  - echo '$KEY' > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys
  - sed -i '' -e 's/^#*PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
  - sysrc sshd_enable=YES
  - service sshd restart
USER
if command -v hdiutil >/dev/null; then
  hdiutil makehybrid -quiet -iso -joliet -default-volume-name cidata -o "$RUN/seed.iso" "$SEED"
else
  genisoimage -quiet -V cidata -J -r -o "$RUN/seed.iso" "$SEED"
fi
rm -rf "$SEED"

SSH=(ssh -q -i "$CACHE/id_ed25519" -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 root@127.0.0.1)

stop_vm() {
  [[ -f "$RUN/qemu.pid" ]] && kill "$(cat "$RUN/qemu.pid")" 2>/dev/null || true
  rm -f "$RUN/qemu.pid"
}

# boot_vm DISK MINUTES: start the VM and wait until root can ssh in.
boot_vm() {
  stop_vm
  rm -f "$RUN/console.log"
  echo "==> booting FreeBSD $VER from $(basename "$1") (console: $RUN/console.log)"
  qemu-system-aarch64 -M virt,highmem=on "${ACCEL[@]}" -smp 4 -m 4096 \
    -bios "$FW" \
    -drive if=virtio,file="$1",format=qcow2 \
    -drive if=virtio,file="$RUN/seed.iso",format=raw,readonly=on \
    -netdev user,id=n0,hostfwd=tcp:127.0.0.1:$PORT-:22 -device virtio-net-pci,netdev=n0 \
    -display none -serial file:"$RUN/console.log" -daemonize -pidfile "$RUN/qemu.pid"
  local tries=$(( $2 * 6 ))
  for i in $(seq "$tries"); do
    "${SSH[@]}" true 2>/dev/null && return 0
    # Progress every 5 minutes (the first boot updates the base system).
    (( i % 30 == 0 )) && echo "    still booting: $(tr -d '\r' < "$RUN/console.log" | grep -v '^$' | tail -1 | cut -c1-100)"
    sleep 10
  done
  echo "ssh did not come up within $2 minutes; console:"; tail -40 "$RUN/console.log"
  return 1
}

# Wait for the VM to power off after `poweroff`.
wait_halt() {
  for _ in $(seq 120); do
    [[ -f "$RUN/qemu.pid" ]] && kill -0 "$(cat "$RUN/qemu.pid")" 2>/dev/null || { rm -f "$RUN/qemu.pid"; return 0; }
    sleep 2
  done
  stop_vm
}

trap stop_vm EXIT

# --- provision once
PROV=$CACHE/freebsd-$VER-provisioned.qcow2
if [[ ! -f "$PROV" ]]; then
  echo "==> provisioning (first run only; the base-system update and Rust download take a while)"
  rm -f "$RUN/provision.qcow2"
  qemu-img create -q -f qcow2 -b "$CACHE/$IMG" -F qcow2 "$RUN/provision.qcow2" 24G
  boot_vm "$RUN/provision.qcow2" 120
  "${SSH[@]}" 'env ASSUME_ALWAYS_YES=yes pkg install -y rust bash tmux >/dev/null && rustc --version'
  "${SSH[@]}" 'poweroff' || true
  wait_halt
  qemu-img convert -O qcow2 "$RUN/provision.qcow2" "$PROV.tmp" && mv "$PROV.tmp" "$PROV"
  rm -f "$RUN/provision.qcow2"
  echo "==> provisioned image saved: $PROV"
fi

# --- test run on a fresh overlay
rm -f "$RUN/disk.qcow2"
qemu-img create -q -f qcow2 -b "$PROV" -F qcow2 "$RUN/disk.qcow2"
boot_vm "$RUN/disk.qcow2" 15
[[ "${PAKDECK_VM_KEEP:-}" == 1 ]] && trap - EXIT
"${SSH[@]}" 'uname -sr; rustc --version'

echo "==> copying the source"
# macOS tar adds Apple metadata (com.apple.provenance) FreeBSD cannot restore.
TAR_NO_META=()
[[ "$(uname -s)" == Darwin ]] && TAR_NO_META=(--no-xattrs --no-mac-metadata)
COPYFILE_DISABLE=1 tar "${TAR_NO_META[@]}" -C "$ROOT" --exclude ./target --exclude ./fuzz/target \
  --exclude ./fuzz/corpus --exclude ./fuzz/artifacts --exclude ./tests/vm/.cache --exclude ./.git -czf - . |
  "${SSH[@]}" 'rm -rf /root/pakdeck && mkdir /root/pakdeck && tar --no-xattrs -C /root/pakdeck -xzf -'

echo "==> cargo build and cargo test on FreeBSD"
"${SSH[@]}" 'cd /root/pakdeck && cargo build --release --locked 2>&1 | tail -2 && install -m 755 target/release/pakdeck /usr/local/bin/pakdeck'
if ! "${SSH[@]}" 'cd /root/pakdeck && cargo test --locked 2>&1' > "$RUN/cargo-test.log"; then
  grep -E "^test .*FAILED|panicked|^test result" "$RUN/cargo-test.log" | head -20
  echo "freebsd VM: FAIL (unit tests)"
  exit 1
fi
grep -E "^test result: ok. [1-9]" "$RUN/cargo-test.log"

echo "==> integration checks"
if "${SSH[@]}" 'export TERM=xterm-256color; bash /root/pakdeck/tests/docker/check.sh' 2>&1 | sed 's/^/[freebsd] /'; then
  echo "freebsd VM: PASS"
else
  echo "freebsd VM: FAIL"
  exit 1
fi
