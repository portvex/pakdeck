#!/usr/bin/env bash
# Run the libFuzzer targets in fuzz/ (requires nightly and cargo-fuzz).
#
# Usage: scripts/fuzz.sh [SECONDS_PER_TARGET] [TARGET...]
#   scripts/fuzz.sh            # every target for 60 s
#   scripts/fuzz.sh 600 ui     # the UI target for 10 minutes
#
# New interesting inputs go to fuzz/corpus/<target>; checked-in seeds in
# fuzz/seeds/<target> are read too. Crashes land in fuzz/artifacts/<target>;
# reproduce with: scripts/fuzz.sh is a thin wrapper around
#   cargo +nightly fuzz run <target> <artifact-file>
set -euo pipefail
cd "$(dirname "$0")/.."

# Pin the nightly toolchain explicitly: a Homebrew cargo/rustc earlier in
# PATH would otherwise be picked up and reject the sanitizer flags.
nightly() {
  RUSTC="$(rustup which --toolchain nightly rustc)" \
    "$(rustup which --toolchain nightly cargo)" "$@"
}

secs=${1:-60}
shift || true
if [[ $# -gt 0 ]]; then
  targets=("$@")
else
  read -r -a targets <<<"$(nightly fuzz list | tr '\n' ' ')"
fi

for t in "${targets[@]}"; do
  echo "==> fuzzing $t for ${secs}s"
  dirs=("fuzz/corpus/$t")
  mkdir -p "${dirs[0]}"
  [[ -d "fuzz/seeds/$t" ]] && dirs+=("fuzz/seeds/$t")
  nightly fuzz run "$t" "${dirs[@]}" -- -max_total_time="$secs" -timeout=10
done
echo "all fuzz targets ran clean"
