#!/usr/bin/env bash
# Compare Lua 5.1-compatible workloads across luaurs, C++ Luau, and LuaJIT.
# Override LUAURS_BIN, LUAU_REF, LUAJIT_BIN, REPEATS, or INNER_RUNS.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export ROOT
export LUAURS_BIN="${LUAURS_BIN:-$ROOT/target/release/luau.exe}"
export LUAU_REF="${LUAU_REF:-$ROOT/../luau-master (1)/luau-reference-build/luau.exe}"
export LUAJIT_BIN="${LUAJIT_BIN:-$ROOT/../LuaJIT-official/src/luajit.exe}"
export REPEATS="${REPEATS:-5}"
export INNER_RUNS="${INNER_RUNS:-5}"
python - <<'PY'
import json
import os
import re
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

root = Path(os.environ["ROOT"])
repeats = int(os.environ["REPEATS"])
inner_runs = int(os.environ["INNER_RUNS"])
commands = {
    "luaurs JIT": (os.environ["LUAURS_BIN"], {"LUAU_TRACE": "1", "LUAU_TRACE_LOG": "1"}),
    "C++ Luau": (os.environ["LUAU_REF"], {}),
    "LuaJIT 2.1": (os.environ["LUAJIT_BIN"], {}),
}
for label, (command, _) in commands.items():
    if not Path(command).is_file():
        raise SystemExit(f"missing {label} executable: {command}")

benchmarks = [
    ("mandel", "bench/tests/shootout/mandel.lua", ["128"]),
    ("binary-trees", "bench/tests/shootout/binary-trees.lua", ["12"]),
    ("heapsort", "bench/tests/shootout/heapsort.lua", ["12", "12000"]),
    ("spectral-norm", "bench/tests/shootout/spectral-norm.lua", ["180"]),
    ("n-body", "bench/tests/shootout/n-body.lua", []),
    ("fannkuch-redux", "bench/tests/shootout/fannkuch-redux.lua", ["9"]),
]
loader = re.compile(r"^local function prequire\(name\).*\n^local bench = .*\n", re.M)
runner_template = '''
arg = __ARGS__
math.randomseed(12345)
local bench = { runCode = function(test, name)
    local total = 0
    for _ = 1, __INNER_RUNS__ do
        if collectgarbage then pcall(collectgarbage, "collect") end
        local started = os.clock()
        local result = test()
        total = total + (result or (os.clock() - started))
    end
    print("BENCH_RESULT\\t" .. name .. "\\t" .. tostring(total / __INNER_RUNS__))
end }
'''

with tempfile.TemporaryDirectory(prefix="luaurs-luajit-bench-") as td:
    temp = Path(td)
    prepared = []
    for name, relative, args in benchmarks:
        source = (root / relative).read_text(encoding="utf-8")
        runner = runner_template.replace("__ARGS__", "{" + ", ".join(json.dumps(arg) for arg in args) + "}")
        runner = runner.replace("__INNER_RUNS__", str(inner_runs))
        source, count = loader.subn(runner, source, count=1)
        if count != 1:
            raise SystemExit(f"could not replace benchmark loader in {relative}")
        path = temp / f"{name}.lua"
        path.write_text(source, encoding="utf-8")
        prepared.append((name, path))

    results = {name: {bench: [] for bench, _ in prepared} for name in commands}
    outputs = {bench: {} for bench, _ in prepared}
    trace_events = {event: 0 for event in ("compiled", "side_exit", "abort")}
    for bench, script in prepared:
        for sample in range(repeats):
            for vm, (executable, extra_env) in commands.items():
                started = time.perf_counter()
                env = os.environ.copy()
                env.update(extra_env)
                proc = subprocess.run(
                    [executable, str(script)],
                    env=env,
                    cwd=root / "bench",
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                    timeout=240,
                )
                wall_ms = (time.perf_counter() - started) * 1000.0
                if proc.returncode:
                    raise SystemExit(f"{vm}/{bench} failed (exit {proc.returncode}):\n{proc.stdout[-4000:]}")
                match = re.search(r"^BENCH_RESULT\t[^\t]+\t([0-9.eE+-]+)\s*$", proc.stdout, re.M)
                if not match:
                    raise SystemExit(f"{vm}/{bench} emitted no timing result:\n{proc.stdout[-4000:]}")
                if vm == "luaurs JIT":
                    for line in proc.stdout.splitlines():
                        if line.startswith("[TRACE] compiled "):
                            trace_events["compiled"] += 1
                        elif line.startswith("[TRACE] side_exit "):
                            trace_events["side_exit"] += 1
                        elif line.startswith("[TRACE] abort "):
                            trace_events["abort"] += 1
                output = "\n".join(
                    line for line in proc.stdout.splitlines()
                    if not line.startswith(("BENCH_RESULT\t", "[TRACE] "))
                ).strip()
                if sample == 0:
                    outputs[bench][vm] = output
                elif outputs[bench][vm] != output:
                    raise SystemExit(f"{vm}/{bench} output changed between samples")
                results[vm][bench].append(float(match.group(1)) * 1000.0)
                print(f"sample {sample + 1}/{repeats}: {bench} on {vm}: process {wall_ms:.1f} ms", flush=True)

    if trace_events["compiled"] == 0:
        raise SystemExit("luaurs emitted no compiled trace events; refusing to label these timings as JIT")

    for bench, _ in prepared:
        reference_output = outputs[bench]["luaurs JIT"]
        for vm in ("C++ Luau", "LuaJIT 2.1"):
            if outputs[bench][vm] != reference_output:
                raise SystemExit(
                    f"{bench} correctness mismatch: luaurs JIT vs {vm}\n"
                    f"luaurs JIT: {reference_output!r}\n{vm}: {outputs[bench][vm]!r}"
                )

    print("\nMedian benchmark execution time (ms; lower is better)")
    print("| Benchmark | luaurs JIT | C++ Luau | LuaJIT 2.1 |")
    print("|---|---:|---:|---:|")
    for bench, _ in prepared:
        medians = [statistics.median(results[vm][bench]) for vm in commands]
        print(f"| {bench} | {medians[0]:.3f} | {medians[1]:.3f} | {medians[2]:.3f} |")
    print(f"\nSamples per VM: {repeats}; executions averaged per process: {inner_runs}; outputs matched exactly.")
    print("luaurs trace events across samples: " + ", ".join(f"{event}={count}" for event, count in trace_events.items()))
PY
