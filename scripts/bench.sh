#!/usr/bin/env bash
# bench.sh DATA [RUNS [BINARY [NEW_RESULTS_DIR]]]
set -euo pipefail
data=$(realpath "${1:?provide an input directory}")
runs=${2:-3}
repo=$(cd "$(dirname "$0")/.." && pwd)
binary=$(realpath "${3:-$repo/target/release/sessions}")
[[ $runs =~ ^[1-9][0-9]*$ && -x $binary ]]
if [[ $# -ge 4 ]]; then
  results=$4
  mkdir -- "$results"
else
  mkdir -p "$repo/out"
  results=$(mktemp -d "$repo/out/bench.XXXXXX")
fi
echo "Results in $results"
cat "$data"/*.csv "$data"/{links,clicks,events}/*.csv >/dev/null
for ((i=1; i<=runs; i++)); do
  run="$results/$i"
  mkdir "$run"
  if /usr/bin/time -o "$run/time.tmp" -f '%e %M' env DATA_DIR="$data" OUTPUT_DIR="$run" NCPU=8 taskset -c 0-7 "$binary" 2>"$run/stages.log"; then
    mv "$run/time.tmp" "$run/time.txt"
    cat "$run/time.txt"
  else
    rm -f -- "$run/time.tmp"
    cat "$run/stages.log" >&2
    exit 1
  fi
done
