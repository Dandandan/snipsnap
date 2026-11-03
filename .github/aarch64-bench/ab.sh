#!/bin/bash
# Benchmarks the compare example at several commits and source variants, for
# each target in $TARGETS, in alternating rounds.
#
# usage: ab.sh SNAPPY_PREFIX HITS_PARQUET WORKDIR
#   REFS      "name:commit name:commit"; HEAD is always included as "head"
#   TARGETS   "name:rustflags;name:rustflags" (default "plain:")
#   ROUNDS    benchmark rounds (default 3)
#   CPU       core to pin to (default 1)
#   OPS       "decode encode" (default) or one of them
#   VARIANTS  glob of the variants/ directories to include (default all);
#             each holds files that replace the same paths under src/ at HEAD
#
# Runs on Linux and macOS (no pinning there). A build that cannot run on the
# host, such as one for a CPU with instructions the host lacks, is reported
# and skipped.
#
# Every side is extracted to the same directory and its files touched before
# it is built, so that cargo cannot mistake one side's build for another's.
set -euo pipefail
mkdir -p "$3"; SNAPPY=$(realpath "$1"); HITS=$(realpath "$2"); W=$(realpath "$3")
REPO=$(git rev-parse --show-toplevel)
CPU=${CPU:-1}; ROUNDS=${ROUNDS:-3}
export LD_LIBRARY_PATH="$SNAPPY/lib" DYLD_LIBRARY_PATH="$SNAPPY/lib"
export SNIPSNAP_PARQUET_BENCH=$HITS SNAPPY_CPP_PREFIX=$SNAPPY
PIN=""; if command -v taskset > /dev/null; then PIN="taskset -c $CPU"; fi
IFS=';' read -ra targets <<< "${TARGETS:-plain:}"
SIDES=""
build_side() { # build_side NAME COMMIT [VARIANT_DIR]
  local src="$W/src"
  rm -rf "$src"; mkdir -p "$src"
  git -C "$REPO" archive "$2" | tar -x -C "$src"
  [ -n "${3:-}" ] && cp -r "$3/." "$src/src/"
  find "$src" -type f -exec touch {} +
  for t in "${targets[@]}"; do
    (cd "$src" && RUSTFLAGS="${t#*:}" cargo build -q --release --example compare \
      --target-dir "$W/t-${t%%:*}")
    cp "$W/t-${t%%:*}/release/examples/compare" "$W/compare-$1-${t%%:*}"
    echo "== built $1-${t%%:*} ($2${3:+ + $(basename "$3")}, RUSTFLAGS=${t#*:})" \
      "$(cksum < "$W/compare-$1-${t%%:*}" | cut -d' ' -f1)"
  done
  SIDES="$SIDES $1"
}
for r in ${REFS:-}; do build_side "${r%%:*}" "${r#*:}"; done
build_side head HEAD
for d in "$REPO"/.github/aarch64-bench/variants/${VARIANTS:-*}/; do
  [ -d "$d" ] && build_side "$(basename "$d")" HEAD "$d"
done
for round in $(seq "$ROUNDS"); do
  for t in "${targets[@]}"; do
    for side in $SIDES; do
      b="$side-${t%%:*}"
      for op in ${OPS:-decode encode}; do
        echo "-- $b $op (round $round)"
        $PIN "$W/compare-$b" --$op --samples 8 | tail -26 || echo "!! $b $op failed"
      done
    done
  done
done
