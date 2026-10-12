#!/usr/bin/env python3
"""Foreground latency and drain progress per time bucket for one run.

usage: timeline.py <out-dir> <label> [bucket-seconds=30]

Seconds are counted from the moment the deadlines came due (negative =
before), so runs line up with each other.
"""
import csv
import json
import sys
from collections import defaultdict

out, label = sys.argv[1], sys.argv[2]
bucket = int(sys.argv[3]) if len(sys.argv) > 3 else 30
r = json.load(open(f"{out}/{label}.json"))
lat = list(csv.DictReader(open(f"{out}/{label}-latency.csv")))
smp = list(csv.DictReader(open(f"{out}/{label}-samples.csv")))
# Older runs didn't store t_due; recover it from the first sample where the
# moved rows became visible (the UPDATE runs due-in = 60s before).
due_rows = [float(s["t"]) for s in smp if int(s["pending"]) + int(s["fired"]) > 0]
t_due = r.get("t_due") or (due_rows[0] + 60 - 0.5 if due_rows and r["variant"] != "none"
                           else float(lat[0]["t"]) + 60)


def key(t):
    return int((t - t_due) // bucket) * bucket


lb = defaultdict(list)
for x in lat:
    if x["status"] == "200":
        lb[key(float(x["t"]))].append(float(x["ms"]))
sb = {}
for s in smp:
    sb[key(float(s["t"]))] = s
prev = 0
print(f"{'t':>6} {'n':>5} {'p50':>7} {'p95':>7} {'p99':>7} {'max':>7} {'fired':>6} {'+fired':>6} {'rssMB':>6} {'lag':>4}")
for k in sorted(lb):
    v = sorted(lb[k])
    s = sb.get(k)
    fired = int(s["fired"]) if s else 0
    print(f"{k:6d} {len(v):5d} {v[len(v) // 2]:7.1f} {v[int(.95 * len(v))]:7.1f} "
          f"{v[int(.99 * len(v))]:7.1f} {v[-1]:7.1f} {fired:6d} {fired - prev:6d} "
          f"{int(s['rss_kb']) // 1024 if s else 0:6d} {s['async_lag'] if s else '':>4}")
    prev = fired
