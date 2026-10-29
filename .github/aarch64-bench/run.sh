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
DBENCH_PAGES="$W/hits_pages.bin" DBENCH_CORPUS="$REPO/tests/rust-snappy-data" SAMPLES=${SAMPLES:-30} \
  taskset -c $CPU "$W/target-dbench/release/dbench"
