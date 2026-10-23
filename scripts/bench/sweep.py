# SPDX-License-Identifier: AGPL-3.0-only
"""Reads write-latency bench logs and says what each slow append overlapped.

Each log is one run of crates/tocsin-store/tests/write_latency.rs under
scripts/bench/fsync_shim.c with TOCSIN_SYNCTRACE=1: a CI job log from
`gh run view --job <id> --log`, or the test's own output. The bench prints
TRACE lines (its appends over 10 ms and the store's slow paths, in ms since
the store opened); the shim prints a SYNCTRACE line per sync, and the bench's
sync of /dev/null as it opens the store is the origin those are read against.

    python3 scripts/bench/sweep.py job-*.log

prints a markdown table, one row per run, then each append over the budget
with the rewinds and log syncs it overlapped. An append is a probe result's
caller, answered once its result is queued; the changing results' callers
and every insert's wait for its commit are reported beside it, so the
disk's stalls stay visible. It exits 1 if any append over the budget
overlapped a rewind or a checkpointer sync of the log (TOC-66).
"""

import re
import sys

BUDGET_MS = 50.0
# Thread names as the kernel keeps them: 15 bytes.
WRITER = "tocsin-store-wr"
CHECKPOINTER = "tocsin-store-ch"

TRACE = re.compile(r"\bTRACE (\S+) (.*)$")
SYNC = re.compile(r"\bSYNCTRACE (\S+) (\S+) (\S+) ([\d.]+) ([\d.]+)")


def read(path):
    """The run in `path`: its summary, samples, events and log syncs."""
    run = {"file": path.rsplit("/", 1)[-1], "summary": None, "samples": {},
           "events": [], "syncs": [], "origin": None}
    for line in open(path, errors="replace"):
        sync = SYNC.search(line)
        if sync:
            what, thread, file, start, took = sync.groups()
            if file == "/dev/null":
                run["origin"] = float(start)
            elif file.endswith("-wal"):
                run["syncs"].append((thread, float(start), float(took)))
            continue
        trace = TRACE.search(line)
        if not trace:
            continue
        kind, rest = trace.group(1), trace.group(2).split()
        if kind == "run":
            run["summary"] = (int(rest[0]), *map(float, rest[1:7]), int(rest[7]))
        elif kind == "event":
            run["events"].append((rest[0], float(rest[1]), float(rest[2])))
        else:
            run["samples"].setdefault(kind, []).append((float(rest[0]), float(rest[1])))
    if run["origin"] is not None:
        run["syncs"] = [(thread, start - run["origin"], took)
                        for thread, start, took in run["syncs"]]
    else:
        run["syncs"] = []
    return run


def overlaps(a, b):
    return a[0] < b[1] and b[0] < a[1]


def analyse(run):
    rewinds = [(f, t) for kind, f, t in run["events"] if kind == "rewind"]
    checkpointer = [(s, s + took) for thread, s, took in run["syncs"]
                    if thread.startswith(CHECKPOINTER)]
    writer = [(s, s + took) for thread, s, took in run["syncs"]
              if thread.startswith(WRITER)]
    slow = []
    for at, took in run["samples"].get("append", []):
        if took <= BUDGET_MS:
            continue
        span = (at, at + took)
        slow.append({
            "at": at, "took": took,
            "rewind": [r for r in rewinds if overlaps(span, r)],
            "checkpointer": [c for c in checkpointer if overlaps(span, c)],
            "writer": [w for w in writer if overlaps(span, w)],
        })
    return {
        "rewinds": rewinds,
        "rewind_max": max((t - f for f, t in rewinds), default=0.0),
        "checkpointer_max": max((t - f for f, t in checkpointer), default=0.0),
        "writer_max": max((t - f for f, t in writer), default=0.0),
        "traced": run["origin"] is not None,
        "slow": slow,
    }


def main(paths):
    runs = [read(path) for path in paths]
    runs = [run for run in runs if run["summary"]]
    print("| run | appends | p99 / max ms | over 50 ms | with a rewind | with a checkpointer log sync"
          " | changes p99 / max ms | commit lag p99 / max ms, over 50 ms"
          " | slow rewinds, longest ms | longest checkpointer log sync ms | longest writer log sync ms |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    bad = 0
    details = []
    for run in runs:
        found = analyse(run)
        samples, p99, worst, change_p99, change_max, lag_p99, lag_max, lagging = run["summary"]
        with_rewind = sum(1 for s in found["slow"] if s["rewind"])
        with_checkpointer = sum(1 for s in found["slow"] if s["checkpointer"])
        bad += sum(1 for s in found["slow"] if s["rewind"] or s["checkpointer"])
        untraced = "" if found["traced"] else " (no sync trace)"
        print(f"| {run['file']}{untraced} | {samples} | {p99:.2f} / {worst:.2f} | {len(found['slow'])}"
              f" | {with_rewind} | {with_checkpointer}"
              f" | {change_p99:.1f} / {change_max:.1f} | {lag_p99:.1f} / {lag_max:.1f}, {lagging}"
              f" | {len(found['rewinds'])}, {found['rewind_max']:.1f}"
              f" | {found['checkpointer_max']:.1f} | {found['writer_max']:.1f} |")
        for s in found["slow"]:
            parts = [f"rewind {t - f:.1f} ms at {f / 1e3:.3f} s" for f, t in s["rewind"]]
            parts += [f"checkpointer log sync {t - f:.1f} ms" for f, t in s["checkpointer"]]
            parts += [f"writer log sync {t - f:.1f} ms" for f, t in s["writer"]]
            details.append(f"- {run['file']}: {s['took']:.1f} ms at {s['at'] / 1e3:.3f} s: "
                           + (", ".join(parts) or "no rewind or log sync"))
    print(f"\n{len(runs)} runs; {bad} appends over {BUDGET_MS:.0f} ms overlapped a rewind"
          " or a checkpointer sync of the log.")
    if details:
        print()
        print("\n".join(details))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
