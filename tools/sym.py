"""Symbolize a LUAU_PROFILE histogram: python sym.py prof.txt [exe] [--lines]"""
import subprocess, sys, collections

prof = sys.argv[1]
import os
exe = sys.argv[2] if len(sys.argv) > 2 and not sys.argv[2].startswith("--") else os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "luau.exe")
lines_mode = '--lines' in sys.argv
rows = []
for ln in open(prof):
    c, a = ln.split()
    rows.append((int(c), a))
total = sum(c for c, _ in rows)
addrs = [a for _, a in rows]
out = subprocess.run(["addr2line", "-f", "-C", "-e", exe] + ([] if True else []),
                     input="\n".join(addrs), capture_output=True, text=True).stdout.splitlines()
byfn = collections.Counter()
byline = collections.Counter()
for i, (c, a) in enumerate(rows):
    fn = out[2 * i] if 2 * i < len(out) else '?'
    loc = out[2 * i + 1] if 2 * i + 1 < len(out) else '?'
    fn = fn.split('::h')[0]
    byfn[fn] += c
    byline[(fn, loc.split('\\')[-1].split('/')[-1])] += c
print(f"total samples {total}")
src = byline if lines_mode else byfn
for k, c in src.most_common(40):
    print(f"{100.0 * c / total:6.2f}% {c:7} {k}")
