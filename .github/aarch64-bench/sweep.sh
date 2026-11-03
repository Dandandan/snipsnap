#!/bin/bash
# Builds the sweep against HEAD and against each variant, and runs each.
# usage: sweep.sh WORKDIR VARIANT...
set -euo pipefail
W=$(mkdir -p "$1" && realpath "$1"); shift
REPO=$(git rev-parse --show-toplevel)
run() { # run NAME [VARIANT_DIR]
  local src="$W/src-$1"; rm -rf "$src"; mkdir -p "$src"
  git -C "$REPO" archive HEAD | tar -x -C "$src"
  [ -n "${2:-}" ] && cp -r "$2/." "$src/src/"
  mkdir -p "$W/sweep-$1/src"; cp "$REPO/.github/aarch64-bench/sweep/main.rs" "$W/sweep-$1/src/"
  printf '[package]\nname = "sweep"\nversion = "0.0.0"\nedition = "2024"\n[dependencies]\nsnipsnap = { path = "%s" }\n[profile.release]\ncodegen-units = 1\n' "$src" > "$W/sweep-$1/Cargo.toml"
  (cd "$W/sweep-$1" && cargo build -q --release)
  echo "== $1"; taskset -c 1 "$W/sweep-$1/target/release/sweep"
}
run head
for v in "$@"; do run "$v" "$REPO/.github/aarch64-bench/variants/$v"; done
