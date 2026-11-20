#!/usr/bin/env python3
"""Summarize tracing-JIT diagnostics from a LUAU_TRACE_LOG=1 run.

Examples:
    LUAU_TRACE_LOG=1 LUAU_TRACE=1 ./target/release/luau program.luau 2>trace.log
    python tools/trace_report.py trace.log
    python tools/trace_report.py --json trace.log
    cat trace.log | python tools/trace_report.py -
"""

import argparse
import json
import re
import sys
from collections import Counter
from contextlib import nullcontext
from pathlib import Path

EVENT_RE = re.compile(r"^\[TRACE\]\s+(\w+)(?:\s+(.*))?$")
FIELD_RE = re.compile(r"([A-Za-z_][A-Za-z_0-9]*)=([^\s]+)")


def parse_log(lines):
    traces = {}
    aborts = Counter()
    exits = Counter()
    totals = Counter()
    abort_pcs = Counter()
    malformed = 0

    for line in lines:
        match = EVENT_RE.match(line.strip())
        if not match:
            continue
        event, payload = match.groups()
        fields = dict(FIELD_RE.findall(payload or ""))
        totals[event] += 1

        if event == "start":
            trace_id = fields.get("id")
            pc = fields.get("pc")
            if trace_id is None or pc is None:
                malformed += 1
                continue
            trace = traces.setdefault(trace_id, {"pc": pc, "compiled": False, "insts": None, "side_exits": 0})
            trace["pc"] = pc
        elif event == "compiled":
            trace_id = fields.get("id")
            pc = fields.get("pc")
            if trace_id is None or pc is None:
                malformed += 1
                continue
            trace = traces.setdefault(trace_id, {"pc": pc, "compiled": False, "insts": None, "side_exits": 0})
            trace.update(pc=pc, compiled=True, insts=_int_or_none(fields.get("insts")))
        elif event == "abort":
            pc = fields.get("pc", "unknown")
            reason = fields.get("reason", "unknown")
            aborts[reason] += 1
            abort_pcs[pc] += 1
        elif event in ("side_exit", "side-exit"):
            trace_id = fields.get("id")
            if trace_id is None:
                malformed += 1
                continue
            trace = traces.setdefault(trace_id, {"pc": "unknown", "compiled": False, "insts": None, "side_exits": 0})
            trace["side_exits"] += 1
            exits[trace_id] += 1

    return {
        "traces": traces,
        "aborts": aborts,
        "abort_pcs": abort_pcs,
        "side_exits": exits,
        "events": totals,
        "malformed_events": malformed,
    }


def _int_or_none(value):
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def render_text(report):
    traces = report["traces"]
    compiled = [(trace_id, data) for trace_id, data in traces.items() if data.get("compiled")]
    aborted = report["abort_pcs"]
    lines = [
        "Tracing JIT report",
        "==================",
        f"Trace starts: {report['events']['start']}",
        f"Compiled traces: {len(compiled)}",
        f"Abort events: {report['events']['abort']}",
    ]

    lines.extend(["", "Traces per loop PC (compiled):"])
    per_pc = Counter(data["pc"] for _, data in compiled)
    if per_pc:
        for pc, count in sorted(per_pc.items(), key=lambda item: (-item[1], item[0])):
            lines.append(f"  pc {pc}: {count}")
    else:
        lines.append("  (none)")

    lines.extend(["", "Aborted loops by PC:"])
    if aborted:
        for pc, count in sorted(aborted.items(), key=lambda item: (-item[1], item[0])):
            lines.append(f"  pc {pc}: {count}")
    else:
        lines.append("  (none)")

    lines.extend(["", "Aborts by reason:"])
    if report["aborts"]:
        for reason, count in report["aborts"].most_common():
            lines.append(f"  {reason}: {count}")
    else:
        lines.append("  (none)")

    lines.extend(["", "Side exits per trace:"])
    if report["side_exits"]:
        for trace_id, count in sorted(report["side_exits"].items(), key=lambda item: (-item[1], item[0])):
            pc = traces.get(trace_id, {}).get("pc", "unknown")
            lines.append(f"  trace {trace_id} (pc {pc}): {count}")
    else:
        lines.append("  (none)")

    if report["malformed_events"]:
        lines.extend(["", f"Malformed trace events: {report['malformed_events']}"])
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", nargs="?", default="-", help="trace log path, or - for stdin (default)")
    parser.add_argument("--json", action="store_true", help="emit machine-readable JSON")
    args = parser.parse_args()

    try:
        lines = sys.stdin if args.log == "-" else Path(args.log).open(encoding="utf-8", errors="replace")
        with lines if args.log != "-" else nullcontext(lines):
            report = parse_log(lines)
    except OSError as error:
        parser.error(str(error))

    if args.json:
        serializable = {
            "traces": report["traces"],
            "aborts_by_reason": dict(report["aborts"]),
            "aborts_by_pc": dict(report["abort_pcs"]),
            "side_exits_by_trace": dict(report["side_exits"]),
            "event_counts": dict(report["events"]),
            "malformed_events": report["malformed_events"],
        }
        print(json.dumps(serializable, indent=2, sort_keys=True))
    else:
        print(render_text(report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
