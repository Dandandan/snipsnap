#!/bin/bash
# Benchmarks decoding on this machine: the compare example at the current
# head and at $BASE_REF, and an in-process comparison of decoder variants
# (the head's src/decode.rs twice, the one at $BASE_REF, and any in
# variants/) against C++ snappy and snap.
#
# usage: run.sh SNAPPY_PREFIX HITS_PARQUET WORKDIR
set -euo pipefail
SNAPPY=$(realpath "$1"); HITS=$(realpath "$2"); W=$(realpath -m "$3")
REPO=$(git rev-parse --show-toplevel); HERE="$REPO/.github/aarch64-bench"
BASE_REF=${BASE_REF:-f4131a1}
CPU=${CPU:-1}
mkdir -p "$W"
export LD_LIBRARY_PATH="$SNAPPY/lib"

echo "== machine"; uname -m; lscpu | grep -E "Model name|Vendor|MHz|L1d|L2|L3" || true
grep -m1 -E "CPU part|CPU implementer" /proc/cpuinfo || true; rustc --version

echo "== pages of $HITS"
cargo build --release -q --manifest-path "$HERE/pqdump/Cargo.toml" --target-dir "$W/target-pqdump"
"$W/target-pqdump/release/pqdump" "$HITS" "$W/hits_pages.bin" | tail -1

echo "== compare example: head and $BASE_REF"
rm -rf "$W/base" && git -C "$REPO" worktree add -f -q "$W/base" "$BASE_REF"
for t in head base; do
  dir=$REPO; [ $t = base ] && dir=$W/base
  (cd "$dir" && SNAPPY_CPP_PREFIX=$SNAPPY cargo build --release -q --example compare --target-dir "$W/target-$t")
  cp "$W/target-$t/release/examples/compare" "$W/compare-$t"
done
for round in 1 2; do
  for t in head base; do
    echo "-- $t (round $round)"
    SNIPSNAP_PARQUET_BENCH=$HITS taskset -c $CPU "$W/compare-$t" --decode --samples 10 | tail -26
  done
done
echo "-- head, encode"
SNIPSNAP_PARQUET_BENCH=$HITS taskset -c $CPU "$W/compare-head" --encode --samples 6 | tail -26

echo "== decoder variants"
D="$W/dbench"; rm -rf "$D" && cp -r "$HERE/dbench" "$D"
cp "$REPO/src/select.rs" "$REPO/src/bytes.rs" "$D/src/"
: > "$D/src/variants.rs"; list=""
add() { cp "$2" "$D/src/v_$1.rs"; echo "#[path = \"v_$1.rs\"] mod v_$1;" >> "$D/src/variants.rs"; list="$list (\"$1\", v_$1::decode_body as fn(&[u8], &mut [MaybeUninit<u8>]) -> Result<()>),"; }
add head "$REPO/src/decode.rs"
git -C "$REPO" show "$BASE_REF:src/decode.rs" > "$W/decode_base.rs"; add base "$W/decode_base.rs"
for f in "$HERE"/variants/*.rs; do [ -e "$f" ] && add "$(basename "$f" .rs)" "$f"; done
add head2 "$REPO/src/decode.rs"
echo "static VARIANTS: &[(&str, fn(&[u8], &mut [MaybeUninit<u8>]) -> Result<()>)] = &[$list];" >> "$D/src/variants.rs"
SNAPPY_LIB="$SNAPPY/lib" cargo build --release -q --manifest-path "$D/Cargo.toml" --target-dir "$W/target-dbench"
export DBENCH_PAGES="$W/hits_pages.bin" DBENCH_CORPUS="$REPO/tests/rust-snappy-data"
SAMPLES=${SAMPLES:-30} taskset -c $CPU "$W/target-dbench/release/dbench"

echo "== perf counters, ClickBench URL pages"
if command -v perf >/dev/null; then
  # The first run decodes nothing: its counts are the setup to subtract.
  for only in head:0 head:30 lmomask2:30 runchk:30 C++:30; do
    DBENCH_ONLY=${only%:*} DBENCH_REPS=${only#*:} perf stat \
      -e cycles,instructions,branch-misses,stalled-cycles-frontend,stalled-cycles-backend,L1-dcache-load-misses \
      taskset -c $CPU "$W/target-dbench/release/dbench" @cb_url 2>&1 | grep -vE "^\s*$" || true
  done
fi

echo "== branch-miss and cycle profiles, ClickBench URL pages"
if command -v perf >/dev/null; then
  for ev in branch-misses cycles; do
    DBENCH_ONLY=head DBENCH_REPS=20 perf record -q -e $ev -o "$W/perf.$ev" \
      taskset -c $CPU "$W/target-dbench/release/dbench" @cb_url > /dev/null 2>&1 || true
    perf report -i "$W/perf.$ev" --stdio --sort sym 2>/dev/null | grep -v "^#" | grep -v "^$" | head -8
    sym=$(perf report -i "$W/perf.$ev" --stdio --sort sym --no-demangle 2>/dev/null | grep -o "_R[A-Za-z0-9_]*v_head11decode_fastKj2_Kb1_[A-Za-z0-9_]*" | head -1)
    echo "annotating $sym"
    if [ -n "$sym" ]; then
      perf annotate -i "$W/perf.$ev" --stdio --no-source "$sym" > "$W/annot.$ev" 2>&1 || true
      # Lines with at least 0.5% of the samples, plus their neighbours.
      awk '$1 ~ /^[0-9]+\.[0-9]+$/ && $1 >= 0.5 {print NR}' "$W/annot.$ev" > "$W/hot.$ev" || true
      awk 'NR==FNR {h[$1]=1; next} (h[FNR-2]||h[FNR-1]||h[FNR]||h[FNR+1]) {print}' "$W/hot.$ev" "$W/annot.$ev" | head -150 || true
      head -5 "$W/annot.$ev" || true
    fi
  done
fi
