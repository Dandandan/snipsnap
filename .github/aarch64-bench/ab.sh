#!/bin/bash
# Benchmarks the compare example at $BASE_REF and at HEAD, for each target
# in $TARGETS, in alternating rounds.
#
# usage: ab.sh SNAPPY_PREFIX HITS_PARQUET WORKDIR
#   BASE_REF  the commit to compare against
#   TARGETS   "name:rustflags;name:rustflags" (default "plain:")
#   ROUNDS    benchmark rounds (default 3)
#   CPU       core to pin to (default 1)
#   OPS       "decode encode" (default) or one of them
#   VARIANTS  glob of the variants/ directories to include (default all)
set -euo pipefail
SNAPPY=$(realpath "$1"); HITS=$(realpath "$2"); W=$(realpath -m "$3")
REPO=$(git rev-parse --show-toplevel)
CPU=${CPU:-1}; ROUNDS=${ROUNDS:-3}
export LD_LIBRARY_PATH="$SNAPPY/lib" SNIPSNAP_PARQUET_BENCH=$HITS SNAPPY_CPP_PREFIX=$SNAPPY
mkdir -p "$W"
IFS=';' read -ra targets <<< "${TARGETS:-plain:}"
# Each variants/NAME/ holds files that replace the same paths under src/ at
# HEAD, benchmarked as one more side.
SIDES="base head"
for d in "$REPO"/.github/aarch64-bench/variants/${VARIANTS:-*}/; do
  [ -d "$d" ] && SIDES="$SIDES $(basename "$d")"
done
for side in $SIDES; do
  ref=HEAD; [ $side = base ] && ref=$BASE_REF
  src="$W/src-$side"; rm -rf "$src"; mkdir -p "$src"
  git -C "$REPO" archive "$ref" | tar -x -C "$src"
  [ -d "$REPO/.github/aarch64-bench/variants/$side" ] && cp -r "$REPO/.github/aarch64-bench/variants/$side/." "$src/src/"
  for t in "${targets[@]}"; do
    (cd "$src" && RUSTFLAGS="${t#*:}" cargo build -q --release --example compare \
      --target-dir "$W/t-${t%%:*}")
    cp "$W/t-${t%%:*}/release/examples/compare" "$W/compare-$side-${t%%:*}"
    echo "== built $side-${t%%:*} ($ref, RUSTFLAGS=${t#*:})"
  done
done
for round in $(seq "$ROUNDS"); do
  for t in "${targets[@]}"; do
    for side in $SIDES; do
      b="$side-${t%%:*}"
      for op in ${OPS:-decode encode}; do
        echo "-- $b $op (round $round)"
        taskset -c "$CPU" "$W/compare-$b" --$op --samples 8 | tail -26
      done
    done
  done
done
