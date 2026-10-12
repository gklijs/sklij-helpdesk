#!/usr/bin/env python3
"""Headline tables from route.py's <label>.json files.

usage: summarize.py <label>.json ...
"""
import json
import sys

SHORT = {"HelpdeskExpiryToTrialLapse": "expiry", "HelpdeskActivationToTrialConversion": "activation",
         "HelpdeskResolutionToArticleSuggestion": "resolution->kb"}


def fmt(s):
    if not s or not s.get("n"):
        return "-"
    return f"{s['p50']:.0f}/{s['p95']:.0f}/{s['p99']:.0f}/{s['max']:.0f} (n={s['n']})"


for path in sys.argv[1:]:
    r = json.load(open(path))
    print(f"== {r['label']}  load before {r['loadavg_before']}  settle {r['settle_after_traffic_s']}"
          f"  peak RSS {r.get('peak_rss_mb')} MB")
    routes = r.get("routes_after_redrive") or r["routes"]
    for name, v in r["routes"].items():
        if name == "ordering":
            continue
        after = routes[name]
        print(f"  {SHORT[name]:15} src {v['sources']} routed {v['routed']} dup {v['duplicates']} "
              f"missing {v['missing']} (parked {v['missing_and_parked']}) -> after redrive: "
              f"routed {after['routed']} dup {after['duplicates']} missing {after['missing']}")
        for ph, s in v["by_phase"].items():
            print(f"      {ph:8} p50/p95/p99/max ms {fmt(s)}")
    print(f"  ordering {r['routes']['ordering']}")
    if "routes_after_redrive" in r:
        print(f"  ordering after redrive {r['routes_after_redrive']['ordering']}")
    for k, s in r["commands"].items():
        if s["n"]:
            print(f"  cmd {k:16} {fmt(s)} fail {s['failures']} rej {s['rejected']}")
    if "recovery" in r:
        print(f"  recovery {r['recovery']}")
    if "redrive" in r:
        print(f"  redrive {r['redrive']}")
