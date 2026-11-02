#!/bin/bash
# Hardware counters for decoding single cases with snipsnap and C++ snappy,
# and a profile of snipsnap's decode loop.
#
# usage: perf.sh SNAPPY_PREFIX WORKDIR
set -uo pipefail
SNAPPY=$(realpath "$1"); W=$(realpath -m "$2")
REPO=$(git rev-parse --show-toplevel)
export LD_LIBRARY_PATH="$SNAPPY/lib" SNAPPY_CPP_PREFIX=$SNAPPY
mkdir -p "$W"
(cd "$REPO" && cargo build -q --release --example compare --target-dir "$W/t")
B=$W/t/release/examples/compare
EV=cycles,instructions,branches,branch-misses,L1-dcache-loads,L1-dcache-load-misses
perf stat -e cycles true > /dev/null 2>&1 || { echo "== perf does not work here"; perf stat -e cycles true; exit 0; }
# Decoding about 1.5 GB per run; the single-iteration run is subtracted.
declare -A ITERS=([urls]=2000 [html]=14000 [txt1]=10000 [pb]=12000 [txt3]=3500)
for c in urls html txt1 pb; do
  for imp in snipsnap cpp; do
    for n in 1 "${ITERS[$c]}"; do
      echo "== $imp $c iters=$n"
      taskset -c 1 perf stat -x, -e "$EV" "$B" --only "decode:$imp:$c" --iters "$n" 2>&1 >/dev/null | grep -v "^$"
    done
    echo "== $imp $c topdown"
    taskset -c 1 perf stat -M TopdownL1 "$B" --only "decode:$imp:$c" --iters "${ITERS[$c]}" 2>&1 >/dev/null | grep -E "%|not supported|rror" | head -12
  done
done
for imp in snipsnap cpp; do
  echo "== profile $imp urls"
  taskset -c 1 perf record -q -o "$W/$imp.data" "$B" --only "decode:$imp:urls" --iters 2000 > /dev/null 2>&1
  perf report -i "$W/$imp.data" --stdio --sort symbol 2>/dev/null | grep -v "^#" | grep -v "^$" | head -8
  echo "== annotate $imp (hottest function)"
  perf annotate -i "$W/$imp.data" --stdio 2>/dev/null | grep -v "^\s*$" | head -450
done
