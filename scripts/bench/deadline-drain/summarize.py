#!/usr/bin/env python3
"""Prints the headline numbers of one or more drain.py runs.

usage: summarize.py <out-dir>/<label>.json ...
"""
import json
import sys

for path in sys.argv[1:]:
    r = json.load(open(path))
    print(f"== {r['label']} ({r['variant']}, fg {r['fg']['workers']}w@{r['fg']['interval_ms']}ms, "
          f"load before {r['loadavg_before'][0]})")
    for k in ["moved", "drain_s", "first_fire_after_s", "fired", "fire_rate_per_s", "head_before",
              "head_after", "max_async_lag", "async_lag_at_drained", "max_lock_waits", "peak_rss_mb"]:
        print(f"  {k}: {r.get(k)}")
    for w in ["latency_before", "latency_during", "latency_after"]:
        if w in r:
            x = r[w]
            print(f"  {w}: n={x['n']} rate={x['rate']}/s p50={x['p50']} p95={x['p95']} p99={x['p99']} "
                  f"max={x['max']} fail={x['failures']} rej={x['rejected']}")
            print(f"    by command [p50, p99]: {x['by_command_p50_p99']}")
    ret = r.get("retention")
    if ret:
        ms = [b["ms"] for b in ret["batches"]]
        print(f"  retention: aged={ret['aged']} rows {ret['rows_before']} -> {ret['rows_after']}, "
              f"{len(ms)} batches, sum {sum(ms):.0f}ms, max batch {max(ms):.1f}ms, wall {ret['wall_s']}s")
        for w in ["fg_before", "fg_during", "fg_after"]:
            x = ret[w]
            print(f"    {w}: n={x['n']} p50={x['p50']} p95={x['p95']} p99={x['p99']} max={x['max']}")
