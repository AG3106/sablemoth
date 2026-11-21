#!/usr/bin/env bash
# fuzz.sh FIRST LAST [EVENTS [BINARY]]
set -euo pipefail
binary=$(realpath "${4:-$(dirname "$0")/../target/release/sessions}")
cd "$(dirname "$0")/.."
gen=vendor/generator/target/release/generate
events=${3:-200000}
[[ $1 =~ ^[0-9]+$ && $2 =~ ^[0-9]+$ && $1 -le $2 ]]
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
run_seed() {
  local seed=$1 dir="$work/$1"
  mkdir "$dir"
  "$gen" --seed "$seed" --events "$events" --clicks "$((events / 4))" --output "$dir/data" --threads 2 >/dev/null
  .venv/bin/python -I ref/ref.py "$dir/data" "$dir/ref"
  DATA_DIR="$dir/data" OUTPUT_DIR="$dir/out" NCPU=4 "$binary"
  .venv/bin/python scripts/compare.py "$dir/ref" "$dir/out"
  rm -rf -- "$dir"
  echo "seed $seed OK"
}
export -f run_seed
export binary gen events work
seq "$1" "$2" | xargs -P "${FUZZ_JOBS:-8}" -I{} bash -euc 'run_seed "$1"' _ {}
