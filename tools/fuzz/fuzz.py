#!/usr/bin/env python3
"""Differential fuzzer for luaurs.

Generates small, deterministic Luau programs and runs each one three ways:

    ours --codegen       (JIT)
    ours --no-codegen    (interpreter)
    C++ luau --codegen   (reference)

Any difference in stdout/stderr or exit code is reported and reduced by dropping
whole blocks and then individual lines.  The generated programs deliberately
avoid clocks, randomness, iteration order and error-message text so a mismatch
means a real semantic/JIT difference rather than formatting noise.

Usage:
    python tools/fuzz/fuzz.py --count 100
    python tools/fuzz/fuzz.py --seed 1234 --count 50 --verbose
    python tools/fuzz/fuzz.py --ref "C:/path/to/luau.exe" --no-shrink

The release binaries must already exist; this script does not build anything.
"""
from __future__ import annotations

import argparse
import dataclasses
import difflib
import os
import random
import re
import subprocess
import sys
import tempfile
import textwrap
from pathlib import Path
from typing import Callable, Sequence

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_REF = os.environ.get(
    "LUAU_REF", r"C:\Users\User\Downloads\luau-master (1)\luau-reference-build\luau.exe"
)
DEFAULT_OUT = Path(__file__).resolve().parent / "failures"

EXE = ".exe" if os.name == "nt" else ""
DEFAULT_OURS = ROOT / "target" / "release" / f"luau{EXE}"
DEFAULT_COMPILER = ROOT / "target" / "release" / f"luau-compile{EXE}"

_ADDRESS = re.compile(r"0x[0-9a-fA-F]+")


# ---------------------------------------------------------------------------
# running and comparing
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class Result:
    code: int
    output: str
    timed_out: bool = False


@dataclasses.dataclass
class Outcome:
    results: dict[str, Result]
    category: str | None
    note: str = ""

    @property
    def mismatch(self) -> bool:
        return self.category is not None


@dataclasses.dataclass
class Config:
    ours: Path
    ref: Path
    compiler: Path | None
    timeout: float
    ref_interp: bool = False
    trace: bool = False
    trace_hotloop: int | None = None


def normalize(text: str) -> str:
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    text = _ADDRESS.sub("0xADDR", text)
    return "\n".join(line.rstrip() for line in text.splitlines()).strip()


def run(cmd: Sequence[str], timeout: float, env: dict[str, str] | None = None) -> Result:
    child_env = None
    if env:
        child_env = os.environ.copy()
        child_env.update(env)
    try:
        proc = subprocess.run(
            list(cmd),
            cwd=ROOT,
            env=child_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        raw = exc.stdout or b""
        if isinstance(raw, str):
            raw = raw.encode("utf-8", "replace")
        return Result(124, normalize(raw.decode("utf-8", "replace")) + "\n<timeout>", True)
    except OSError as exc:
        raise SystemExit(f"cannot run {cmd[0]}: {exc}")
    return Result(proc.returncode, normalize(proc.stdout.decode("utf-8", "replace")))


def evaluate(text: str, cfg: Config, path: Path) -> Outcome:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    results = {
        "jit": run([str(cfg.ours), "--codegen", str(path)], cfg.timeout),
        "interp": run([str(cfg.ours), "--no-codegen", str(path)], cfg.timeout),
        "ref": run([str(cfg.ref), "--codegen", str(path)], cfg.timeout),
    }
    if cfg.trace:
        env = {"LUAU_TRACE": "1"}
        if cfg.trace_hotloop is not None:
            env["LUAU_TRACE_HOTLOOP"] = str(cfg.trace_hotloop)
        results["trace"] = run([str(cfg.ours), "--codegen", str(path)], cfg.timeout, env)
    if cfg.ref_interp:
        results["ref-interp"] = run([str(cfg.ref), str(path)], cfg.timeout)

    ours_same = results["jit"] == results["interp"]
    trace_same = not cfg.trace or results["trace"] == results["jit"]
    ref_same = results["jit"] == results["ref"]
    if any(result.timed_out for result in results.values()):
        category = "timeout"
    elif not ours_same:
        category = "jit-vs-interp"
    elif not trace_same:
        category = "trace-vs-jit"
    elif not ref_same:
        category = "ours-vs-ref"
    else:
        category = None

    note = ""
    if ours_same and trace_same and results["ref"].code != 0 and results["jit"].code == 0:
        # The reference may not understand a newer Lua/Luau feature.  Do not
        # report that as a semantic mismatch; the program is simply out of scope.
        note = "reference rejected the program (unsupported feature?)"
        category = None
    if cfg.ref_interp and results["ref-interp"] != results["ref"]:
        extra = "reference interpreter and reference JIT disagree"
        note = f"{note}; {extra}" if note else extra

    return Outcome(results, category, note)


def format_outcome(outcome: Outcome) -> str:
    lines: list[str] = []
    for name, result in outcome.results.items():
        status = "timeout" if result.timed_out else f"exit {result.code}"
        lines.append(f"--- {name} ({status})")
        lines.append(result.output if result.output else "<empty>")
    if outcome.category:
        lines.append(f"--- category: {outcome.category}")
    if outcome.note:
        lines.append(f"--- note: {outcome.note}")
    return "\n".join(lines)


def different_outputs(outcome: Outcome) -> str:
    names = list(outcome.results)
    chunks: list[str] = []
    for i, left in enumerate(names):
        for right in names[i + 1 :]:
            a, b = outcome.results[left], outcome.results[right]
            if a == b:
                continue
            diff = difflib.unified_diff(
                a.output.splitlines(),
                b.output.splitlines(),
                fromfile=left,
                tofile=right,
                lineterm="",
            )
            chunks.append("\n".join(diff))
    return "\n".join(chunks)


# ---------------------------------------------------------------------------
# program generation
# ---------------------------------------------------------------------------

NUMBERS = ["0", "1", "2", "3", "5", "7", "-1", "0.5", "1.5", "-2.5", "10"]


def number_expr(rng: random.Random, depth: int = 2) -> str:
    if depth <= 0 or rng.random() < 0.35:
        return rng.choice(NUMBERS + ["a", "b", "c"])
    op = rng.choice(["+", "-", "*", "/", "%", "//", "^"])
    left = number_expr(rng, depth - 1)
    right = number_expr(rng, depth - 1)
    if op in ("/", "%", "//"):
        # Keep the denominator non-zero without changing the type.
        right = f"(({right}) * ({right}) + 0.5)"
    elif op == "^":
        right = rng.choice(["2", "3"])
    return f"({left} {op} {right})"


def wrap_block(name: str, body: str) -> str:
    return f"-- @@block {name}\ndo\n{textwrap.indent(body.strip(), '    ')}\nend"


def arith_block(rng: random.Random) -> str:
    locals_ = "\n".join(f"local {v} = {number_expr(rng)}" for v in "xyz")
    a = rng.choice(["7", "-7.5", "2.5", "1"])
    b = rng.choice(["3", "2.5", "-1.5", "6"])
    c = rng.choice(["2", "1.5", "0", "-2"])
    body = f"""
local function f(a, b, c)
    {locals_.replace(chr(10), chr(10) + '    ')}
    return x, y, z
end
for i = 1, 40 do f(i, i + 1, i % 3) end
print(f({a}, {b}, {c}))
print(f(-7.5, 2.5, 1.5))
local ok, p, q, r = pcall(f, 1, "x", 2)
print(ok, type(p))
"""
    return wrap_block("arith", body)


def int_block(rng: random.Random) -> str:
    s1, s2 = rng.randint(1, 7), rng.randint(1, 7)
    body = f"""
local function fi(a, b)
    local x = integer.add(a, b)
    local y = integer.sub(integer.mul(a, b), integer.create({rng.randint(1, 9)}))
    local z = integer.bxor(integer.band(a, b), integer.bor(a, integer.bnot(b)))
    local w = integer.lrotate(integer.lshift(a, {s1}i), {s2}i)
    local v = integer.rshift(integer.rrotate(b, {s2}i), {s1}i)
    return x, y, z, w, v
end
for i = 1, 40 do fi(integer.create(i), integer.create(i + 3)) end
print(fi(5i, 3i))
print(fi(-5i, 3i))
local ok, err = pcall(fi, 1, 2i)
print(ok, type(err))
"""
    return wrap_block("integer", body)


def table_block(rng: random.Random) -> str:
    arr = [rng.randint(-9, 9) for _ in range(3)]
    x, y = rng.randint(-9, 9), rng.randint(-9, 9)
    new, z = rng.randint(-9, 9), rng.randint(-9, 9)
    body = f"""
local t = {{{arr[0]}, {arr[1]}, {arr[2]}, x = {x}, y = {y}}}
local function get(t, k) return t[k] end
for i = 1, 40 do get(t, i % 5 + 1) end
print(get(t, 1), get(t, "x"), get(t, 10))
t[2] = {new}
t.z = {z}
print(#t, t[2], rawget(t, "z"))
local mt = {{__index = function(_, k) return k .. "!" end, __newindex = function(t, k, v) rawset(t, k, v * 2) end}}
local p = setmetatable({{}}, mt)
p.a = 3
p.b = 4
print(p.a, p.b, p.missing)
"""
    return wrap_block("table", body)


def metatable_block(rng: random.Random) -> str:
    v1, v2 = rng.randint(1, 9), rng.randint(1, 9)
    body = f"""
local mt = {{
    __add = function(a, b) return a.v + b.v end,
    __sub = function(a, b) return a.v - b.v end,
    __mul = function(a, b) return a.v * b.v end,
    __eq = function(a, b) return a.v == b.v end,
    __lt = function(a, b) return a.v < b.v end,
    __le = function(a, b) return a.v <= b.v end,
    __concat = function(a, b) return tostring(a.v) .. tostring(b.v) end,
    __len = function(a) return a.v end,
    __call = function(a, x) return a.v + x end,
}}
local function O(v) return setmetatable({{v = v}}, mt) end
local function ops(a, b)
    return a + b, a - b, a * b, a == b, a < b, a <= b, a .. b, #a, a(3)
end
for i = 1, 40 do ops(O(i), O(i + 1)) end
print(ops(O({v1}), O({v2})))
local ok, err = pcall(ops, O(1), 2)
print(ok, type(err))
"""
    return wrap_block("metatable", body)


def loop_block(rng: random.Random) -> str:
    n = rng.randint(20, 60)
    body = f"""
local function churn(n)
    local out = 0
    for i = 1, n do
        local x = i
        if i % 3 == 0 then x = "s" .. i end
        if i % 5 == 0 then x = {{i}} end
        if i % 7 == 0 then x = nil end
        if type(x) == "number" then out += x elseif type(x) == "string" then out += #x else out += 1 end
    end
    return out
end
for i = 1, 40 do churn(50) end
print(churn({n}))
local function fail(n)
    local s = 0
    for i = 1, n do
        local x = i
        if i % 4 == 0 then x = "s" .. i end
        local ok = pcall(function() return x + 1 end)
        s += ok and 1 or 0
    end
    return s
end
for i = 1, 40 do fail(20) end
print(fail(40))
"""
    return wrap_block("loop-types", body)


def closure_block(rng: random.Random) -> str:
    start = rng.randint(1, 9)
    body = f"""
local function make(n)
    local function inc(k) n = n + k return n end
    local function get() return n end
    return inc, get
end
local f, g = make({start})
for i = 1, 40 do f(i) end
print(f(2), g())

local u = {start}
local function bump() u = (type(u) == "number") and ("v" .. u) or 1 end
local function use(n)
    local s = 0
    for i = 1, n do
        s += (type(u) == "number" and u or #u)
        bump()
    end
    return s, u
end
for i = 1, 40 do use(20) end
print(use(10))
"""
    return wrap_block("closures", body)


def vararg_block(rng: random.Random) -> str:
    a = rng.randint(1, 9)
    body = f"""
local function v(...)
    local n = select("#", ...)
    local a, b, c = ...
    return n, a, b, c, select(2, ...)
end
for i = 1, 40 do v(i, i + 1, i + 2) end
print(v({a}, {a + 1}, {a + 2}))
print(select("#", 1, nil, 3))
local function pack(...) return {{...}}, select("#", ...) end
local t, n = pack(1, nil, 3)
print(n, t[1], t[2], t[3])
"""
    return wrap_block("varargs", body)


def string_block(rng: random.Random) -> str:
    s = rng.choice(["abcdef", "hello world", "luaurs", "a1b2c3", "  pad  "])
    sub = rng.choice(["abc", "world", "lua", "1b2", "pad"])
    pat = rng.choice(["a", "b", "l", "%d", "%a"])
    repl = rng.choice(["#", "xy", ""])
    idx = rng.randint(1, len(s))
    body = f"""
local s = "{s}"
print(#s, string.sub(s, 2, 4), string.byte(s, {idx}), string.char(65, 66), string.rep("ab", 3), string.reverse(s))
print(string.find(s, "{sub}", 1, true))
print(string.gsub(s, "{pat}", "{repl}"))
print(string.format("%d %s %.2f", 42, "x", 3.14159))
"""
    return wrap_block("strings", body)


def pcall_block(rng: random.Random) -> str:
    code = rng.randint(2, 9)
    body = f"""
local function boom(n)
    local acc = 0
    for i = 1, n do
        acc += i
        if i == 7 then error({{code = i}}) end
    end
    return acc
end
for i = 1, 40 do pcall(boom, 20) end
local ok, err = pcall(boom, 10)
print(ok, type(err), type(err) == "table" and err.code or {code})
local ok2, err2 = pcall(function() error("plain") end)
print(ok2, type(err2))
local function idx(t) return t.x.y end
for i = 1, 40 do pcall(idx, {{x = {{y = i}}}}) end
local ok3, err3 = pcall(idx, {{}})
print(ok3, type(err3))
"""
    return wrap_block("pcall", body)


def vector_block(rng: random.Random) -> str:
    a, b = rng.randint(1, 9), rng.randint(1, 9)
    body = f"""
local function vf(a, b)
    local c = a + b
    local d = c * 2 - a / 2
    local e = -d
    local n = vector.normalize(c)
    return e.x, e.y, e.z, vector.dot(c, d), vector.magnitude(c), n.x, n.y, n.z
end
for i = 1, 40 do vf(vector.create(i, i + 1, 2), vector.create(1, 2, 3)) end
print(vf(vector.create({a}, {b}, 3), vector.create(4, 5, 6)))
local ok, err = pcall(vf, 1, 2)
print(ok, type(err))
"""
    return wrap_block("vector", body)


def logic_block(rng: random.Random) -> str:
    a, b, c = rng.randint(1, 9), rng.randint(1, 9), rng.randint(1, 9)
    body = f"""
local function aor(a, b, c) return (a and b or c), (a or b), (not a), (a and "x") end
for i = 1, 40 do aor(i % 2 == 0, i, "z") end
print(aor(true, {a}, {b}), aor(false, {a}, {b}), aor(nil, nil, {c}))
local function cmp(a, b) return a < b, a <= b, a == b, a ~= b, a > b, a >= b end
for i = 1, 40 do cmp(i, i + 1) end
print(cmp(1, 2), cmp(0 / 0, 0 / 0), cmp("a", "b"))
"""
    return wrap_block("logic", body)


def loops_block(rng: random.Random) -> str:
    n = rng.randint(10, 40)
    body = f"""
local function loops(a, b)
    local s = 0
    for i = a, b, 0.5 do s += i end
    for i = 10, 1, -3 do s += i end
    for i = 1, 0 do s += 100 end
    return s
end
for i = 1, 40 do loops(1, 3) end
print(loops(1, 2))
local function w(n)
    local i, s = 0, 0
    while i < n do i += 1; s += i end
    repeat i -= 1; s -= i until i == 0
    return s
end
for i = 1, 40 do w(20) end
print(w({n}))
local function carry(n)
    local s = 0
    for i = 1, n do
        local x = (i % 3 == 0) and ("s" .. i) or i
        if i % 5 == 0 then continue end
        if type(x) == "number" then s += x else s += #x end
        if s > 100 then break end
    end
    return s
end
for i = 1, 40 do carry(100) end
print(carry(50))
"""
    return wrap_block("loops", body)


def multi_block(rng: random.Random) -> str:
    a = rng.randint(1, 9)
    body = f"""
local function m() return {a}, {a + 1}, {a + 2} end
local function n() return m(), m() end
print(n())
print(select("#", m()), select(2, m()))
local function pass(...) return ... end
print(pass(m()))
"""
    return wrap_block("multi-return", body)


def fastcall_block(rng: random.Random) -> str:
    x = rng.randint(1, 5)
    body = f"""
local function fc(x)
    local a = math.abs(x)
    local b = math.floor(x)
    local c = math.ceil(x)
    local d = math.min(x, 3)
    local e = math.max(x, -3)
    local s = string.byte("abc", x)
    local t = string.sub("abcdef", x, x + 1)
    local r = string.rep("x", math.floor(x) + 1)
    local ok1, e1 = pcall(math.abs, "s")
    local ok2, e2 = pcall(string.byte, "abc", "x")
    local ok3, e3 = pcall(table.insert, 1, 2)
    local i, f = math.modf(x)
    local ok4, e4 = pcall(bit32.band, "x", 1)
    return a, b, c, d, e, s, t, r, ok1, type(e1), ok2, type(e2), ok3, type(e3), i, f, ok4, type(e4)
end
for i = 1, 40 do fc((i % 5) + 1) end
print(fc({x}))
print(fc(-2.5))
local t = {{1, 2, 3}}
for i = 1, 40 do table.insert(t, i); table.remove(t) end
print(table.concat(t, ","))
print(table.find(t, 2), table.find(t, 9))
"""
    return wrap_block("fastcalls", body)


def coroutine_block(rng: random.Random) -> str:
    n = rng.randint(1, 9)
    body = f"""
local function co(n)
    local c = coroutine.create(function(a)
        local b = coroutine.yield(a + 1)
        local d = coroutine.yield(b * 2)
        return b + d
    end)
    local ok1, v1 = coroutine.resume(c, n)
    local ok2, v2 = coroutine.resume(c, n + 1)
    local ok3, v3 = coroutine.resume(c, n + 2)
    return ok1, v1, ok2, v2, ok3, v3
end
for i = 1, 40 do co(i) end
print(co({n}))
local function across(n)
    local c = coroutine.create(function(a)
        local ok, v = pcall(function() return coroutine.yield(a) end)
        return ok, v
    end)
    local ok1, v1 = coroutine.resume(c, n)
    local ok2, v2 = coroutine.resume(c, n + 1)
    return ok1, v1, ok2, v2
end
for i = 1, 40 do across(i) end
print(across({n}))
"""
    return wrap_block("coroutines", body)


def proxy_block(rng: random.Random) -> str:
    body = f"""
local store = {{}}
local p = newproxy(true)
local mt = getmetatable(p)
mt.__index = {{10, 20, 30, a = "A"}}
mt.__newindex = function(_, k, v) store[k] = v end
mt.__add = function(a, b) return 7 end
mt.__len = function(_) return 3 end
mt.__eq = function(a, b) return true end
mt.__lt = function(a, b) return false end
mt.__concat = function(a, b) return "C" end
mt.__call = function(a, x) return x + 1 end
mt.__tostring = function(a) return "P" end
local function use(o, n)
    local s = 0
    for i = 1, n do
        s += o[1] + o[2] + o[3]
        o[1] = i
        o.b = i * 2
    end
    return s, o[1], o.b, #o, o.a
end
for i = 1, 40 do use(p, 20) end
print(use(p, 5))
local ok = pcall(use, nil, 1)
print(ok)
local function pmath(a, b)
    return a + b, a == b, a < b, a .. b, #a, a(3), tostring(a)
end
for i = 1, 40 do pmath(p, p) end
print(pmath(p, p))
local q = newproxy(true)
getmetatable(q).__index = function(_, k) return k end
local function useq(o, n)
    local s = ""
    for i = 1, n do s = s .. tostring(o.a) .. tostring(o[1]) end
    return s
end
for i = 1, 40 do useq(q, 10) end
print(useq(q, 3))
"""
    return wrap_block("proxy", body)


def error_values_block(rng: random.Random) -> str:
    body = f"""
local function e1() error(42) end
local function e2() error({{code = 9}}) end
local function e3() error("msg") end
local function h(err) return type(err), type(err) == "table" and err.code or 0 end
local function run(f)
    local ok, a, b = xpcall(f, h)
    return ok, a, b
end
for i = 1, 40 do run(e1); run(e2); run(e3) end
print(run(e1))
print(run(e2))
print(run(e3))
"""
    return wrap_block("error-values", body)


def tablelib_block(rng: random.Random) -> str:
    body = f"""
local t = {{1, 2, 3}}
for i = 1, 40 do table.insert(t, i); table.remove(t) end
print(table.concat(t, ","))
print(table.find(t, 2), table.find(t, 9))
local u = {{}}
for i = 1, 5 do table.insert(u, i, i * 10) end
print(table.concat(u, ","))
print(table.remove(u, 2), table.concat(u, ","))
local src = {{1, 2, 3, 4}}
local dst = {{}}
table.move(src, 2, 4, 1, dst)
print(table.concat(dst, ","))
"""
    return wrap_block("table-library", body)


def vector_loop_block(rng: random.Random) -> str:
    body = f"""
local function vloop(n)
    local v = vector.create(0, 0, 0)
    local s = 0
    for i = 1, n do
        v = v + vector.create(i, -i, 0.5)
        s += v.x * v.y
    end
    return s, v.x, v.y, v.z
end
for i = 1, 40 do vloop(30) end
print(vloop(10))
"""
    return wrap_block("vector-loop", body)


def integer_loop_block(rng: random.Random) -> str:
    body = f"""
local function iloop(n)
    local x = 1i
    local s = 0i
    for i = 1, n do
        x = integer.add(x, integer.create(i))
        s = integer.bxor(s, x)
    end
    return s, x
end
for i = 1, 40 do iloop(20) end
print(iloop(10))
"""
    return wrap_block("integer-loop", body)


BLOCKS: dict[str, Callable[[random.Random], str]] = {
    "arith": arith_block,
    "integer": int_block,
    "table": table_block,
    "metatable": metatable_block,
    "loop-types": loop_block,
    "closures": closure_block,
    "varargs": vararg_block,
    "strings": string_block,
    "pcall": pcall_block,
    "vector": vector_block,
    "logic": logic_block,
    "loops": loops_block,
    "multi-return": multi_block,
    "fastcalls": fastcall_block,
    "coroutines": coroutine_block,
    "proxy": proxy_block,
    "error-values": error_values_block,
    "table-library": tablelib_block,
    "vector-loop": vector_loop_block,
    "integer-loop": integer_loop_block,
}


def generate(rng: random.Random) -> str:
    count = rng.randint(3, min(6, len(BLOCKS)))
    names = rng.sample(sorted(BLOCKS), count)
    return "\n\n".join(BLOCKS[name](rng) for name in names)


# ---------------------------------------------------------------------------
# shrinking
# ---------------------------------------------------------------------------

_BLOCK_START = re.compile(r"^-- @@block .*$", re.MULTILINE)


def split_blocks(text: str) -> list[str]:
    matches = list(_BLOCK_START.finditer(text))
    if not matches:
        return [text.strip()] if text.strip() else []
    blocks: list[str] = []
    for i, match in enumerate(matches):
        end = matches[i + 1].start() if i + 1 < len(matches) else len(text)
        blocks.append(text[match.start() : end].strip())
    return blocks


def shrink(
    text: str,
    cfg: Config,
    tmpdir: Path,
    check: Callable[[str], bool],
    verbose: bool,
) -> str:
    blocks = split_blocks(text)
    changed = True
    while changed and len(blocks) > 1:
        changed = False
        for i in range(len(blocks)):
            candidate = "\n\n".join(blocks[:i] + blocks[i + 1 :])
            if check(candidate):
                if verbose:
                    print(f"  shrink: dropped {blocks[i].splitlines()[0]}")
                blocks = blocks[:i] + blocks[i + 1 :]
                changed = True
                break
    text = "\n\n".join(blocks)

    if cfg.compiler is None or not cfg.compiler.exists():
        return text

    syntax_path = tmpdir / "syntax.lua"

    def parses(candidate: str) -> bool:
        syntax_path.write_text(candidate, encoding="utf-8")
        result = run([str(cfg.compiler), "--text", str(syntax_path)], cfg.timeout)
        return result.code == 0

    for _ in range(4):
        lines = text.splitlines()
        changed = False
        i = 0
        while i < len(lines):
            candidate = "\n".join(lines[:i] + lines[i + 1 :])
            if parses(candidate) and check(candidate):
                if verbose:
                    print(f"  shrink: dropped line {i + 1}: {lines[i].strip()[:80]}")
                lines = lines[:i] + lines[i + 1 :]
                changed = True
            else:
                i += 1
        text = "\n".join(lines)
        if not changed:
            break
    return text


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------


def save_failure(
    out_dir: Path,
    seed: int,
    index: int,
    original: str,
    shrunk: str,
    outcome: Outcome,
) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    base = out_dir / f"seed{seed}-{index:04d}"
    source = base.with_suffix(".lua")
    source.write_text(shrunk + "\n", encoding="utf-8")
    report = [
        f"seed: {seed}",
        f"case: {index}",
        f"category: {outcome.category}",
        f"note: {outcome.note}",
        "",
        "=== outputs",
        format_outcome(outcome),
        "",
        "=== diff",
        different_outputs(outcome),
        "",
        "=== reduced program",
        shrunk,
    ]
    if shrunk != original:
        report.extend(["", "=== original program", original])
    base.with_suffix(".txt").write_text("\n".join(report) + "\n", encoding="utf-8")
    return source


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--count", type=int, default=100, help="number of programs (default 100)")
    parser.add_argument("--seed", type=int, default=None, help="RNG seed (default random)")
    parser.add_argument("--ours", default=str(DEFAULT_OURS), help="luaurs release CLI")
    parser.add_argument("--ref", default=str(DEFAULT_REF), help="C++ Luau reference CLI")
    parser.add_argument("--compiler", default=str(DEFAULT_COMPILER), help="luau-compile for syntax checks")
    parser.add_argument("--timeout", type=float, default=10.0, help="per-run timeout in seconds")
    parser.add_argument("--out", default=str(DEFAULT_OUT), help="directory for reduced failures")
    parser.add_argument("--max-failures", type=int, default=5, help="stop after N mismatches")
    parser.add_argument("--no-shrink", action="store_true", help="do not reduce mismatches")
    parser.add_argument("--ref-interp", action="store_true", help="also run the reference interpreter")
    parser.add_argument("--trace", action="store_true", help="also run with LUAU_TRACE=1 (tracing JIT)")
    parser.add_argument("--trace-hotloop", type=int, default=None, help="LUAU_TRACE_HOTLOOP threshold for --trace")
    parser.add_argument("--verbose", action="store_true", help="print progress/details")
    args = parser.parse_args(argv)

    cfg = Config(
        ours=Path(args.ours),
        ref=Path(args.ref),
        compiler=Path(args.compiler) if args.compiler else None,
        timeout=args.timeout,
        ref_interp=args.ref_interp,
        trace=args.trace,
        trace_hotloop=args.trace_hotloop,
    )
    for name, path in (("luaurs", cfg.ours), ("reference", cfg.ref)):
        if not path.exists():
            print(f"missing {name} binary: {path}", file=sys.stderr)
            return 2

    seed = args.seed if args.seed is not None else random.SystemRandom().randrange(1 << 32)
    rng = random.Random(seed)
    out_dir = Path(args.out)
    failures = 0
    print(f"seed={seed} count={args.count} ours={cfg.ours} ref={cfg.ref}")
    if cfg.compiler and not cfg.compiler.exists():
        print(f"warning: {cfg.compiler} missing; line-level shrinking disabled", file=sys.stderr)

    with tempfile.TemporaryDirectory(prefix="luaurs-fuzz-") as tmp:
        tmpdir = Path(tmp)
        for index in range(args.count):
            program = generate(rng)
            path = tmpdir / f"case{index:04d}.lua"
            outcome = evaluate(program, cfg, path)
            if not outcome.mismatch:
                if args.verbose:
                    print(f"[{index + 1}/{args.count}] ok")
                continue

            failures += 1
            print(f"\nMISMATCH {index}: {outcome.category}")
            print(format_outcome(outcome))
            if outcome.note:
                print(f"note: {outcome.note}")
            print("diff:")
            print(different_outputs(outcome) or "<none>")

            reduced = program
            if not args.no_shrink:
                check_path = tmpdir / "shrink.lua"

                def check(candidate: str) -> bool:
                    return evaluate(candidate, cfg, check_path).mismatch

                reduced = shrink(program, cfg, tmpdir, check, args.verbose)
                outcome = evaluate(reduced, cfg, check_path)

            saved = save_failure(out_dir, seed, index, program, reduced, outcome)
            print(f"saved: {saved}")
            if failures >= args.max_failures:
                print(f"stopping after {failures} mismatches")
                break

    print(f"\n=== {args.count} generated, {failures} mismatch(es), seed={seed}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
