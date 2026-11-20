#!/usr/bin/env bash
# Differential tests: every script in the suite must print the same with the
# interpreter, the regular JIT and (with TRACE=1) the tracing JIT.
#
#   bash tools/jitdiff.sh                      # tests/jit, interpreter vs JIT
#   SUITE=tests/trace bash tools/jitdiff.sh    # tracing-JIT stress scripts
#   TRACE=1 SUITE=tests/trace bash tools/jitdiff.sh
#
# The first line of a test may carry extra CLI flags:
#   -- jitflags: --fflags=...
cd "$(dirname "$0")/.."
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) EXE_SUFFIX=.exe ;;
  *) EXE_SUFFIX= ;;
esac
BIN=${BIN:-./target/release/luau${EXE_SUFFIX}}
SUITE=${SUITE:-tests/jit}
TRACE=${TRACE:-0}
fail=0
norm() { sed -E 's/0x[0-9a-fA-F]+/ADDR/g'; }
for f in "$SUITE"/*.lua; do
  # A test may need extra CLI flags (for example fflags for experimental syntax).
  flags=""
  first=$(head -n 1 "$f")
  case "$first" in
    "-- jitflags:"*) flags="${first#-- jitflags: }" ;;
  esac
  a=$(timeout 60 "$BIN" $flags --no-codegen "$f" 2>&1); ra=$?
  b=$(timeout 60 "$BIN" $flags --codegen "$f" 2>&1); rb=$?
  # table/function addresses differ between runs
  a=$(printf '%s\n' "$a" | norm); b=$(printf '%s\n' "$b" | norm)
  if [ "$a" == "$b" ] && [ $ra -eq $rb ]; then
    if [ "$TRACE" = 1 ]; then
      c=$(LUAU_TRACE=1 timeout 60 "$BIN" $flags --codegen "$f" 2>&1); rc=$?
      c=$(printf '%s\n' "$c" | norm)
      if [ "$a" == "$c" ] && [ $ra -eq $rc ]; then
        echo "ok   $f (trace)"
      else
        echo "DIFF $f trace (exit $ra vs $rc)"
        fail=1
        diff <(printf '%s\n' "$a") <(printf '%s\n' "$c") | head -8
      fi
    else
      echo "ok   $f"
    fi
  else
    echo "DIFF $f (exit $ra vs $rb)"
    fail=1
    diff <(printf '%s\n' "$a") <(printf '%s\n' "$b") | head -8
  fi
done
exit $fail
