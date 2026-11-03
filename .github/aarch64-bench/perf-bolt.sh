#!/bin/bash
# Hardware counters for compression with the plain and BOLT builds made by
# pgo-bolt.sh in WORKDIR. usage: perf-bolt.sh WORKDIR
set -uo pipefail
W=$(realpath "$1")
for c in html pb urls text_4k_blocks txt1; do
  for b in "$W/t-plain/release/examples/compare:plain" "$W/compare.bolt:bolt"; do
    echo "== perf $c ${b##*:}"
    taskset -c 1 perf stat -x, -e cycles,instructions,branches,branch-misses,stall_frontend,stall_backend,L1-icache-load-misses,br_retired \
      "${b%:*}" --only "encode:snipsnap:$c" --iters 400 2>&1 >/dev/null | grep -v "^$" | cut -d, -f1,3 | tr '\n' ' '
    echo
  done
done
# Where the mispredicted branches are: samples of branch-misses by function
# and offset, for the cases BOLT sped up and slowed down.
for c in html pb urls text_4k_blocks; do
  for b in "$W/t-plain/release/examples/compare:plain" "$W/compare.bolt:bolt"; do
    echo "== misses $c ${b##*:}"
    taskset -c 1 perf record -q -e branch-misses:u -c 200 -o "$W/miss.data" \
      "${b%:*}" --only "encode:snipsnap:$c" --iters 400 > /dev/null 2>&1
    perf script -i "$W/miss.data" -F sym,symoff 2>/dev/null | awk '{print $1}' \
      | sort | uniq -c | sort -rn | head -25
  done
done
