#!/bin/bash
# Hardware counters for one case at a time, snipsnap against C++, if the
# runner exposes them. Each count is printed for 1 and for 201 iterations;
# the difference is 200 calls. usage: x86-perf.sh COMPARE_BINARY
set -uo pipefail
B=$1
echo "== perf availability"
perf stat -e cycles,instructions,branch-misses true 2>&1 | grep -E "cycles|instructions|branch|supported|rror" | head -5
GROUPS_=(
  "cycles,instructions,branches,branch-misses"
  "L1-dcache-loads,L1-dcache-load-misses,L1-icache-load-misses"
  "ls_stlf,ls_bad_status2.stli_other,de_no_dispatch_per_slot.no_ops_from_frontend,de_no_dispatch_per_slot.backend_stalls"
)
for op in encode decode; do
  for c in txt1 txt2 html pb gaviota urls logs_1m; do
    for imp in snipsnap cpp; do
      for g in "${GROUPS_[@]}"; do
        for n in 1 201; do
          echo "-- $op $c $imp $n $(taskset -c 1 perf stat -x, -e "$g" "$B" --only "$op:$imp:$c" --iters $n 2>&1 >/dev/null \
            | grep -v '^$' | cut -d, -f1,3 | tr '\n' ' ')"
        done
      done
    done
  done
done
echo "== topdown"
for c in txt1 html; do
  for imp in snipsnap cpp; do
    echo "-- encode $c $imp"
    taskset -c 1 perf stat -M PipelineL1 "$B" --only "encode:$imp:$c" --iters 201 2>&1 >/dev/null | grep -E "%|rror" | head -8
  done
done
