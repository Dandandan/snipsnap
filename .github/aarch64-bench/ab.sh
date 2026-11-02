#!/bin/bash
# Benchmarks the compare example at $BASE_REF and at HEAD, for each target
# in $TARGETS, in alternating rounds.
#
# usage: ab.sh SNAPPY_PREFIX HITS_PARQUET WORKDIR
#   BASE_REF  the commit to compare against
#   TARGETS   "name:rustflags;name:rustflags" (default "plain:")
#   ROUNDS    benchmark rounds (default 3)
#   CPU       core to pin to (default 1)
set -euo pipefail
SNAPPY=$(realpath "$1"); HITS=$(realpath "$2"); W=$(realpath -m "$3")
REPO=$(git rev-parse --show-toplevel)
CPU=${CPU:-1}; ROUNDS=${ROUNDS:-3}
export LD_LIBRARY_PATH="$SNAPPY/lib" SNIPSNAP_PARQUET_BENCH=$HITS SNAPPY_CPP_PREFIX=$SNAPPY
mkdir -p "$W"
IFS=';' read -ra targets <<< "${TARGETS:-plain:}"
for side in base head; do
  ref=HEAD; [ $side = base ] && ref=$BASE_REF
  src="$W/src-$side"; rm -rf "$src"; mkdir -p "$src"
  git -C "$REPO" archive "$ref" | tar -x -C "$src"
  for t in "${targets[@]}"; do
    (cd "$src" && RUSTFLAGS="${t#*:}" cargo build -q --release --example compare \
      --target-dir "$W/t-${t%%:*}")
    cp "$W/t-${t%%:*}/release/examples/compare" "$W/compare-$side-${t%%:*}"
    echo "== built $side-${t%%:*} ($ref, RUSTFLAGS=${t#*:})"
  done
done
for round in $(seq "$ROUNDS"); do
  for t in "${targets[@]}"; do
    for side in base head; do
      b="$side-${t%%:*}"
      echo "-- $b decode (round $round)"
      taskset -c "$CPU" "$W/compare-$b" --decode --samples 10 | tail -26
      echo "-- $b encode (round $round)"
      taskset -c "$CPU" "$W/compare-$b" --encode --samples 6 | tail -26
    done
  done
done
