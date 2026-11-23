#!/usr/bin/env bash
# Full conformance sweep. Each test runs with a wall-clock timeout and, where
# supported, an address-space cap so a runaway test cannot exhaust available RAM.
cd "$(dirname "$0")"
PLATFORM=$(uname -s)
case "$PLATFORM" in
  MINGW*|MSYS*|CYGWIN*) EXE_SUFFIX=.exe ;;
  *) EXE_SUFFIX= ;;
esac
BIN=${BIN:-./target/release/luau${EXE_SUFFIX}}
TIMEOUT_BIN=${TIMEOUT_BIN:-timeout}
if ! command -v "$TIMEOUT_BIN" >/dev/null 2>&1; then
  if command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_BIN=gtimeout
  else
    echo "sweep.sh requires timeout (install GNU coreutils on macOS)" >&2
    exit 2
  fi
fi
pass=0; fail=0; failed=()
for f in tests/conformance/*.luau; do
  name=$(basename "$f" .luau)
  ( if [ "$PLATFORM" != Darwin ]; then ulimit -v ${VMEM_KB:-4000000} 2>/dev/null || true; fi
    "$TIMEOUT_BIN" ${TO:-60} "$BIN" "$f" >"/tmp/out_$name.txt" 2>&1 )
  code=$?
  if [ $code -eq 0 ]; then
    echo "PASS $name"; pass=$((pass+1))
  else
    echo "FAIL $name (exit $code)"; fail=$((fail+1)); failed+=("$name")
    tail -n 5 "/tmp/out_$name.txt" | sed 's/^/    | /'
  fi
done
echo "=== $pass passed, $fail failed ==="
echo "failing: ${failed[*]}"
exit "$fail"
