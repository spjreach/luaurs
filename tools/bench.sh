#!/usr/bin/env bash
# usage: bench.sh <tag> [timeout]  -- runs every bench script on the Rust port and the reference
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
R="${RBIN:-$ROOT/target/release/luau.exe}"; C="${LUAU_REF:-$ROOT/../luau-reference-build/luau.exe}"
TO="${2:-60}"; D="${BOUT:-/tmp/bout}"; mkdir -p "$D"
cd "$ROOT/bench"
export LUAU_BENCH_RUNS=1
for f in $(find tests micro_tests gc other -name "*.lua" -o -name "*.luau" | sort); do
  n=$(echo "$f" | tr '/' '_')
  s=$(date +%s%N); timeout "$TO" "$R" $FLAGS "$f" >"$D/r_$n.txt" 2>&1; rc=$?; rt=$(( ($(date +%s%N)-s)/1000000 ))
  s=$(date +%s%N); timeout "$TO" "$C" $FLAGS "$f" >"$D/c_$n.txt" 2>&1; cc=$?; ct=$(( ($(date +%s%N)-s)/1000000 ))
  echo "$f rust=$rc/${rt}ms ref=$cc/${ct}ms"
done
