#!/bin/bash
# Builds the compare example in several configurations (CPU targets, LTO,
# PGO, BOLT and their combinations), then benchmarks each one, also under
# run-time settings, in alternating rounds.
#
# usage: matrix.sh SNAPPY_PREFIX HITS_PARQUET WORKDIR
#   CPUS     space-separated target CPUs to try, e.g. "x86-64-v3 native"
#   ROUNDS   benchmark rounds (default 2)
#   CPU      core to pin to (default 1)
#   ONLY     space-separated configuration names to keep (default: all)
#   BOLT_DIR directory with llvm-bolt and merge-fdata (default: newest found)
set -euo pipefail
SNAPPY=$(realpath "$1"); HITS=$(realpath "$2"); W=$(realpath -m "$3")
REPO=$(git rev-parse --show-toplevel)
CPU=${CPU:-1}; ROUNDS=${ROUNDS:-2}
SYSROOT_BIN=$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin
PROFDATA=${LLVM_PROFDATA:-$SYSROOT_BIN/llvm-profdata}
BOLT_DIR=${BOLT_DIR:-$(dirname "$(ls /usr/lib/llvm-*/bin/llvm-bolt 2>/dev/null | sort -V | tail -1 || true)")}
export LD_LIBRARY_PATH="$SNAPPY/lib" SNIPSNAP_PARQUET_BENCH=$HITS SNAPPY_CPP_PREFIX=$SNAPPY
mkdir -p "$W"
RELOCS="-Clink-args=-Wl,--emit-relocs"
LTO_ENV="CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1"
want() { [ -z "${ONLY:-}" ] || [[ " $ONLY " == *" $1 "* ]]; }

build() { # build NAME RUSTFLAGS [ENV...]; prints the binary
  local name=$1 flags=$2; shift 2
  (cd "$REPO" && env "$@" RUSTFLAGS="$flags $RELOCS" \
    cargo build -q --release --example compare --target-dir "$W/t-$name")
  echo "$W/t-$name/release/examples/compare"
}
train() { "$1" --decode --samples 2 > /dev/null; "$1" --encode --samples 2 > /dev/null; }
pgo() { # pgo NAME RUSTFLAGS [ENV...]; prints the PGO binary
  local name=$1 flags=$2; shift 2
  rm -rf "$W/raw-$name"
  local gen; gen=$(build "$name-gen" "$flags -Cprofile-generate=$W/raw-$name" "$@")
  train "$gen"
  "$PROFDATA" merge -o "$W/$name.profdata" "$W/raw-$name"/*.profraw
  build "$name" "$flags -Cprofile-use=$W/$name.profdata -Cllvm-args=-no-pgo-warn-mismatch" "$@"
}
bolt() { # bolt INPUT OUTPUT; fails if BOLT is missing or fails
  [ -x "$BOLT_DIR/llvm-bolt" ] || return 1
  rm -f "$W/bolt.fdata"*
  "$BOLT_DIR/llvm-bolt" "$1" -instrument -o "$1.instr" --instrumentation-file="$W/bolt.fdata" \
    --instrumentation-file-append-pid > "$W/bolt-instr.log" 2>&1 || { tail -3 "$W/bolt-instr.log"; return 1; }
  train "$1.instr" || return 1
  "$BOLT_DIR/merge-fdata" "$W"/bolt.fdata* > "$W/merged.fdata" 2>/dev/null || return 1
  "$BOLT_DIR/llvm-bolt" "$1" -o "$2" -data="$W/merged.fdata" -reorder-blocks=ext-tsp \
    -reorder-functions=cdsort -split-functions -split-all-cold -split-eh -dyno-stats \
    > "$W/bolt-opt.log" 2>&1 || { tail -3 "$W/bolt-opt.log"; return 1; }
}

RUNS=() # entries "label|binary|runtime env"
add() { RUNS+=("$1|$2|${3:-}"); echo "== built $1"; }
THP="GLIBC_TUNABLES=glibc.malloc.hugetlb=1"

PLAIN=$(build plain ""); add plain "$PLAIN"
want thp && add thp "$PLAIN" "$THP"
want lto && add lto "$(build lto "" $LTO_ENV)"
for c in ${CPUS:-}; do
  want "$c" && add "$c" "$(build "cpu-$c" "-Ctarget-cpu=$c")"
done
if want pgo || want pgo+bolt || want all; then PGO=$(pgo pgo ""); want pgo && add pgo "$PGO"; fi
if want bolt && bolt "$PLAIN" "$W/plain.bolt"; then add bolt "$W/plain.bolt"; fi
if { want pgo+bolt || want all; } && bolt "$PGO" "$W/pgo.bolt"; then
  want pgo+bolt && add pgo+bolt "$W/pgo.bolt"
fi
# Everything at once: the last CPU target, fat LTO, PGO, BOLT, huge pages.
if want all; then
  last=$(echo ${CPUS:-} | awk '{print $NF}')
  ALLPGO=$(pgo all "${last:+-Ctarget-cpu=$last}" $LTO_ENV)
  if bolt "$ALLPGO" "$W/all.bolt"; then add all "$W/all.bolt" "$THP"; else add all "$ALLPGO" "$THP"; fi
fi

for round in $(seq "$ROUNDS"); do
  for r in "${RUNS[@]}"; do
    IFS='|' read -r label bin envs <<< "$r"
    echo "-- $label decode (round $round)"
    env $envs taskset -c "$CPU" "$bin" --decode --samples 10 | tail -26
    echo "-- $label encode (round $round)"
    env $envs taskset -c "$CPU" "$bin" --encode --samples 6 | tail -26
  done
done
