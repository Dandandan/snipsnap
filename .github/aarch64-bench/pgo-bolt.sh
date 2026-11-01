#!/bin/bash
# Builds the compare example four ways (plain, PGO, BOLT, PGO+BOLT), trains
# PGO and BOLT on its own decode and encode runs, and prints the benchmark for
# each build.
#
# usage: pgo-bolt.sh SNAPPY_PREFIX HITS_PARQUET WORKDIR
#   LLVM_PROFDATA  llvm-profdata matching rustc's LLVM (default: from llvm-tools)
#   BOLT_DIR       directory with llvm-bolt and merge-fdata (default /usr/lib/llvm-18/bin)
#   CPU            core to pin to (default 1)
#   EXTRA          more plain builds, as "name:rustflags;name:rustflags"
set -euo pipefail
SNAPPY=$(realpath "$1"); HITS=$(realpath "$2"); W=$(realpath -m "$3")
REPO=$(git rev-parse --show-toplevel); HERE="$REPO/.github/aarch64-bench"
CPU=${CPU:-1}
SYSROOT_BIN=$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin
PROFDATA=${LLVM_PROFDATA:-$SYSROOT_BIN/llvm-profdata}
BOLT_DIR=${BOLT_DIR:-$(dirname "$(ls /usr/lib/llvm-*/bin/llvm-bolt 2>/dev/null | tail -1 || true)")}
export LD_LIBRARY_PATH="$SNAPPY/lib" SNIPSNAP_PARQUET_BENCH=$HITS SNAPPY_CPP_PREFIX=$SNAPPY
mkdir -p "$W"
RELOCS="-Clink-args=-Wl,--emit-relocs"
build() { # build TARGETDIR RUSTFLAGS
  (cd "$REPO" && RUSTFLAGS="$2" cargo build -q --release --example compare --target-dir "$W/$1")
  echo "$W/$1/release/examples/compare"
}
train() { "$1" --decode --samples 2 > /dev/null; "$1" --encode --samples 2 > /dev/null; }
bolt() { # bolt INPUT OUTPUT; returns non-zero if BOLT fails
  rm -f "$W/bolt.fdata"*
  "$BOLT_DIR/llvm-bolt" "$1" -instrument -o "$1.instr" --instrumentation-file="$W/bolt.fdata" \
    --instrumentation-file-append-pid > "$W/bolt-instr.log" 2>&1 || { tail -5 "$W/bolt-instr.log"; return 1; }
  train "$1.instr" || return 1
  "$BOLT_DIR/merge-fdata" "$W"/bolt.fdata* > "$W/merged.fdata" 2>/dev/null || return 1
  "$BOLT_DIR/llvm-bolt" "$1" -o "$2" -data="$W/merged.fdata" -reorder-blocks=ext-tsp \
    -reorder-functions=cdsort -split-functions -split-all-cold -split-eh -dyno-stats \
    > "$W/bolt-opt.log" 2>&1 || { tail -5 "$W/bolt-opt.log"; return 1; }
  grep -E "BOLT-INFO: .*(functions with profile|basic block reordering)" "$W/bolt-opt.log" | head -3 || true
}
echo "== plain"; PLAIN=$(build t-plain "$RELOCS")
echo "== PGO"
rm -rf "$W/pgo-raw"; GEN=$(build t-pgo-gen "-Cprofile-generate=$W/pgo-raw $RELOCS")
train "$GEN"; "$PROFDATA" merge -o "$W/pgo.profdata" "$W"/pgo-raw/*.profraw
PGO=$(build t-pgo "-Cprofile-use=$W/pgo.profdata -Cllvm-args=-no-pgo-warn-mismatch $RELOCS")
BUILDS="$PLAIN:plain $PGO:pgo"
if [ -x "$BOLT_DIR/llvm-bolt" ]; then
  echo "== BOLT ($BOLT_DIR)"
  bolt "$PLAIN" "$W/compare.bolt" && BUILDS="$BUILDS $W/compare.bolt:bolt"
  echo "== PGO+BOLT"
  bolt "$PGO" "$W/compare.pgo.bolt" && BUILDS="$BUILDS $W/compare.pgo.bolt:pgo+bolt"
else
  echo "== no llvm-bolt found; skipping BOLT"
fi
# EXTRA="name:rustflags;name:rustflags" adds plain builds with extra flags.
IFS=';' read -ra extras <<< "${EXTRA:-}"
for e in "${extras[@]}"; do
  echo "== ${e%%:*}: ${e#*:}"
  BUILDS="$BUILDS $(build "t-${e%%:*}" "${e#*:} $RELOCS"):${e%%:*}"
done
# Each variants/NAME/ holds files that replace the same paths under src/ in
# a plain build of the committed tree.
for d in "$HERE"/variants/*/; do
  [ -d "$d" ] || continue
  n=$(basename "$d"); src="$W/src-$n"; rm -rf "$src"; mkdir -p "$src"
  git -C "$REPO" archive HEAD | tar -x -C "$src"
  cp -r "$d". "$src/src/"
  echo "== variant $n: $(cd "$d" && ls | tr '\n' ' ')"
  (cd "$src" && RUSTFLAGS="$RELOCS" cargo build -q --release --example compare --target-dir "$W/t-v-$n")
  BUILDS="$BUILDS $W/t-v-$n/release/examples/compare:$n"
done
[ -n "${NOBENCH:-}" ] && exit 0
for round in 1 2; do
  for b in $BUILDS; do
    echo "-- ${b##*:} decode (round $round)"; taskset -c $CPU "${b%:*}" --decode --samples 10 | tail -26
    echo "-- ${b##*:} encode (round $round)"; taskset -c $CPU "${b%:*}" --encode --samples 6 | tail -26
  done
done
