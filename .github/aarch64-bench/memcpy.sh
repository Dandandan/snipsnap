#!/bin/bash
# Why do long literals decode slowly on some x86 runners? Prints glibc's
# memcpy tuning and decodes the literal-heavy cases under tunables that turn
# off non-temporal stores and `rep movsb` for large copies.
#
# usage: memcpy.sh SNAPPY_PREFIX WORKDIR
set -uo pipefail
SNAPPY=$(realpath "$1"); W=$(realpath -m "$2")
REPO=$(git rev-parse --show-toplevel)
export LD_LIBRARY_PATH="$SNAPPY/lib" SNAPPY_CPP_PREFIX=$SNAPPY
mkdir -p "$W"
LDSO=$(ls /lib64/ld-linux-x86-64.so.2 /lib/ld-linux-aarch64.so.1 2>/dev/null | head -1)
echo "== tunables"; "$LDSO" --list-tunables 2>&1 | grep -iE "x86_|non_temporal|rep_|cache" | head -30
echo "== diagnostics"; "$LDSO" --list-diagnostics 2>&1 | grep -iE "cache|threshold|rep_movsb|non_temporal|cpu_features.preferred|isa_1" | head -40
(cd "$REPO" && cargo build -q --release --example compare --target-dir "$W/t")
B=$W/t/release/examples/compare
for t in "" "glibc.cpu.x86_non_temporal_threshold=0x7fffffffffff" "glibc.cpu.x86_rep_movsb_threshold=0x7fffffffffff" "glibc.cpu.x86_non_temporal_threshold=0x7fffffffffff:glibc.cpu.x86_rep_movsb_threshold=0x7fffffffffff"; do
  echo "== GLIBC_TUNABLES=$t"
  GLIBC_TUNABLES=$t taskset -c 1 "$B" --decode --filter literal_64k --filter pdf --filter jpg --samples 10 | tail -4
done
