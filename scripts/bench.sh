#!/usr/bin/env bash
# usage: scripts/bench.sh DATA [RUNS]  — run pinned to 8 hyperthreads, print stage log of last run
D=$1; N=${2:-3}
cd "$(dirname "$0")/.."
cat $D/*/* > /dev/null
for i in $(seq $N); do rm -rf out/bench; /usr/bin/time -f "wall %e s  maxrss %M KB" env DATA_DIR=$D OUTPUT_DIR=out/bench NCPU=8 taskset -c 0-7 ./target/release/sessions 2>out/bench.log; tail -1 out/bench.log; done
grep -v wall out/bench.log
