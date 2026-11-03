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
