#!/usr/bin/env bash
# usage: scripts/fuzz.sh FIRST LAST [EVENTS]   — Rust vs ref.py on generated seeds
set -u
cd "$(dirname "$0")/.."
GEN=vendor/generator/target/release/generate
E=${3:-200000}; C=$((E / 4))
W=$(mktemp -d -p "${TMPDIR:-/tmp}")
fail=0
run_seed() {
  s=$1
  $GEN --seed $s --events $E --clicks $C --output $W/d$s --threads 2 >/dev/null
  .venv/bin/python -I ref/ref.py $W/d$s $W/r$s
  DATA_DIR=$W/d$s OUTPUT_DIR=$W/o$s NCPU=4 ./target/release/sessions 2>/dev/null
  if .venv/bin/python scripts/compare.py $W/r$s $W/o$s > $W/c$s 2>&1; then echo "seed $s OK"; else echo "seed $s MISMATCH"; cat $W/c$s; fi
  rm -rf $W/d$s $W/r$s $W/o$s
}
export -f run_seed; export GEN E C W
seq $1 $2 | xargs -P 8 -I{} bash -c 'run_seed {}' | sort -V | tee $W/summary
grep -q MISMATCH $W/summary && fail=1
rm -rf $W
exit $fail
