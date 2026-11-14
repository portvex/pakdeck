#!/usr/bin/env bash
# Install each native package from DIR in its own distribution container,
# check the files and `pakdeck --version`, then remove it again.
#
# Usage: tests/packages/run.sh DIR [FORMAT ...]
#   FORMAT: deb deb-old rpm rpm-suse arch apk (default: all)
#   DIR holds the packages from scripts/package.sh (amd64 / x86_64).
# Installs with the commands the README shows users.
set -euo pipefail
cd "$(dirname "$0")/../.."
if [[ $# -lt 1 ]]; then
  echo "usage: tests/packages/run.sh DIR [deb|deb-old|rpm|rpm-suse|arch|apk ...]" >&2
  exit 2
fi
dir=$(cd "$1" && pwd)
shift
formats=("$@")
[[ ${#formats[@]} -eq 0 ]] && formats=(deb deb-old rpm rpm-suse arch apk)

# One package per format; the glob must match exactly one file.
pick() {
  # shellcheck disable=SC2206 # $1 is a glob pattern and must expand here
  local matches=("$dir"/$1)
  [[ ${#matches[@]} -eq 1 && -f "${matches[0]}" ]] ||
    { echo "packages: expected one $1 in $dir" >&2; return 1; }
  basename "${matches[0]}"
}

# Checks after install and after removal (POSIX sh in the container, so the
# $ expressions are meant for that shell). LICENSE is the license file's path
# for the format; /usr/share/doc is not checked further because slim images
# (debian:*-slim) drop it apart from copyright files.
# shellcheck disable=SC2016
check='
  set -e
  test -x /usr/bin/pakdeck
  test -f "$LICENSE"
  out=$(pakdeck --version)
  echo "$out"
  case "$out" in pakdeck\ *) ;; *) echo "unexpected --version: $out"; exit 1 ;; esac
'
gone='
  if [ -e /usr/bin/pakdeck ]; then echo "/usr/bin/pakdeck left after removal"; exit 1; fi
'
deb_license=/usr/share/doc/pakdeck/copyright
license=/usr/share/licenses/pakdeck/LICENSE

rc=0
for f in "${formats[@]}"; do
  case "$f" in
    deb | deb-old)
      img=debian:stable-slim
      [[ $f == deb-old ]] && img=ubuntu:20.04 # dpkg 1.19: no zstd
      pkg=$(pick 'pakdeck_*_amd64.deb') lic=$deb_license
      install="apt-get install -y -qq ./$pkg" remove="apt-get remove -y -qq pakdeck" ;;
    rpm)
      img=fedora:latest pkg=$(pick 'pakdeck-*.x86_64.rpm') lic=$license
      install="dnf install -y -q ./$pkg" remove="dnf remove -y -q pakdeck" ;;
    rpm-suse)
      img=opensuse/tumbleweed:latest pkg=$(pick 'pakdeck-*.x86_64.rpm') lic=$license
      install="zypper --non-interactive --no-refresh install --allow-unsigned-rpm ./$pkg"
      remove="zypper --non-interactive remove pakdeck" ;;
    arch)
      img=archlinux:latest pkg=$(pick 'pakdeck-*-x86_64.pkg.tar.zst') lic=$license
      # pacman's Landlock sandbox cannot be applied inside Docker.
      install="sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf && pacman -U --noconfirm ./$pkg"
      remove="pacman -R --noconfirm pakdeck" ;;
    apk)
      img=alpine:latest pkg=$(pick 'pakdeck_*_x86_64.apk') lic=$license
      install="apk add --no-network --allow-untrusted ./$pkg" remove="apk del pakdeck" ;;
    *) echo "packages: unknown format $f" >&2; exit 2 ;;
  esac
  echo "==> $f: $pkg in $img"
  if docker run --rm -v "$dir":/pkgs:ro -w /pkgs -e LICENSE="$lic" -e DEBIAN_FRONTEND=noninteractive \
      "$img" sh -c "$install && ($check) && $remove && ($gone)" 2>&1 |
      sed "s/^/[$f] /"; then
    echo "$f: PASS"
  else
    echo "$f: FAIL"; rc=1
  fi
done
exit $rc
