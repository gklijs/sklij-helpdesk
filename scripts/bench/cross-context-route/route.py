#!/usr/bin/env python3
"""One cross-context route run (issue #29). See README.md in this directory.

usage: route.py <server-binary> <template-db|-> <label> <out-dir> [options]

Copies <template-db> (or starts from an empty database with `-`), starts a
release server and drives two kinds of traffic through phases:

- **probes**: lifecycle walks on fresh companies - SignUpCompany,
  ExpireCompanyTrial, (gap), ReactivateCompany - at --probe-rate companies/s.
  Each CompanyExpired / CompanyActivated is routed into marketing as
  TrialLapsed / TrialConverted by skilj's CrossContextRoute loop.
- **foreground**: the deadline-drain ticket walk (create, assign, note, note,
  resolve), --fg-workers clients paced at --fg-interval-ms, during phases
  that ask for it. Every TicketResolved is routed into knowledge_base as
  ArticleSuggested (HelpdeskResolutionToArticleSuggestion).

--phases is a comma list of name:seconds[:flags]; flags are `load` (foreground
on) and `fault` (the --fault is in force for the phase). Example:
  idle:120,load:180:load,fault:120:load+fault,recover:180:load

Faults (marketing unless --fault-context knowledge_base):
  outage       BEFORE INSERT trigger on the target's events raising an error
  flaky:P      the same trigger, raising with probability P (0..1)
  slow:MS      a trigger that pg_sleep()s MS per inserted event, then succeeds
  lock         one transaction holding the target's sequence row FOR UPDATE

Latency is commit to commit: the routed event's metadata_created_at minus the
source event's, matched by company_id (lifecycle) or ticket_id (resolution).
Afterwards the run waits for every route to reach the head, optionally
redrives every parked delivery over GraphQL (--redrive), and checks exact
counts: each source event routed exactly once, parked, or missing.
"""
import argparse
import csv
import http.client
import json
import os
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "deadline-drain"))
from common import (PGBIN, PGHOST, PGPORT, PGUSER, Client, Server, createdb,  # noqa: E402
                    dropdb, psql, summary, ticket_payload)

ap = argparse.ArgumentParser()
ap.add_argument("binary")
ap.add_argument("template")
ap.add_argument("label")
ap.add_argument("out")
ap.add_argument("--phases", default="idle:120,load:180:load,tail:60")
ap.add_argument("--probe-rate", type=float, default=2.0, help="companies per second")
ap.add_argument("--probe-threads", type=int, default=8)
ap.add_argument("--gap-ms", type=float, default=1000, help="expire -> reactivate gap")
ap.add_argument("--fg-workers", type=int, default=8)
ap.add_argument("--fg-interval-ms", type=int, default=250)
ap.add_argument("--fault", default=None)
ap.add_argument("--fault-context", default="marketing")
ap.add_argument("--settle-max", type=float, default=900, help="max wait for routes after phases")
ap.add_argument("--redrive", action="store_true")
ap.add_argument("--port", type=int, default=18194)
ap.add_argument("--warmup", type=float, default=20, help="seconds idle after server start")
args = ap.parse_args()

DB = f"cr_run_{args.label}".replace("-", "_")
PREFIX = f"cr-{args.label}-"
ROUTES = {  # route -> (source event, target context, target event, match key)
    "HelpdeskExpiryToTrialLapse": ("CompanyExpired", "marketing", "TrialLapsed", "company_id"),
    "HelpdeskActivationToTrialConversion": ("CompanyActivated", "marketing", "TrialConverted", "company_id"),
    "HelpdeskResolutionToArticleSuggestion": ("TicketResolved", "knowledge_base", "ArticleSuggested", "ticket_id"),
}
stop_all = threading.Event()
fg_on = threading.Event()
probe_on = threading.Event()
latencies = []  # (t, kind, command, status, accepted, ms)
lat_lock = threading.Lock()
samples = []
marks = {}
probe_log = []  # (company, t_signup, t_expire, t_reactivate, ok)


def phases():
    out = []
    for p in args.phases.split(","):
        bits = p.split(":")
        flags = set(bits[2].split("+")) if len(bits) > 2 else set()
        out.append((bits[0], float(bits[1]), flags))
    return out


def record(kind, command, t, res):
    status, accepted, ms = res
    with lat_lock:
        latencies.append((t, kind, command, status, accepted, ms))


def call(client, kind, command, payload):
    t = time.time()
    try:
        res = client.trigger(command, payload)
    except Exception:  # noqa: BLE001 - a transport failure, counted
        res = (-1, False, (time.time() - t) * 1000)
    record(kind, command, t, res)
    return res


def prober(k, server):
    client = Client(server.port, server.tokens)
    interval = args.probe_threads / args.probe_rate
    nxt = time.time() + interval * k / args.probe_threads
    n = 0
    while not stop_all.is_set():
        if not probe_on.is_set():
            time.sleep(0.05)
            nxt = time.time() + interval * k / args.probe_threads
            continue
        delay = nxt - time.time()
        if delay > 0:
            time.sleep(delay)
        nxt += interval
        cid = f"{PREFIX}{k}-{n}"
        n += 1
        t0 = time.time()
        a = call(client, "probe", "SignUpCompany",
                 {"company_id": cid, "name": cid, "contact_email": "p@x.example"})
        t1 = time.time()
        b = call(client, "probe", "ExpireCompanyTrial", {"company_id": cid})
        time.sleep(args.gap_ms / 1000)
        t2 = time.time()
        c = call(client, "probe", "ReactivateCompany", {"company_id": cid})
        with lat_lock:
            probe_log.append((cid, t0, t1, t2, all(r[0] == 200 and r[1] for r in (a, b, c))))


def fg_worker(w, server):
    client = Client(server.port, server.tokens)
    company = f"{PREFIX}fg-{w}"
    call(client, "fg", "SignUpCompany", {"company_id": company, "name": company,
                                         "contact_email": "fg@x.example"})
    interval = args.fg_interval_ms / 1000
    n = 0
    while not stop_all.is_set():
        if not fg_on.is_set():
            time.sleep(0.05)
            continue
        nxt = time.time() + interval * w / args.fg_workers
        tid = f"{PREFIX}{w}-{n}"
        n += 1
        steps = [
            ("CreateTicket", ticket_payload(tid, company)),
            ("AssignTicket", {"ticket_id": tid, "staff_id": "fg-staff"}),
            ("AddInternalNote", {"ticket_id": tid, "staff_id": "fg-staff", "note": "a"}),
            ("AddInternalNote", {"ticket_id": tid, "staff_id": "fg-staff", "note": "b"}),
            ("ResolveTicket", {"ticket_id": tid}),
        ]
        # A started ticket is always walked to its resolve, so every
        # created ticket has one TicketResolved to route.
        for command, payload in steps:
            delay = nxt - time.time()
            if delay > 0:
                time.sleep(delay)
            nxt += interval
            call(client, "fg", command, payload)


SAMPLE_SQL = """
SELECT
 (SELECT next_value FROM bc_helpdesk.sequence),
 (SELECT coalesce(json_object_agg(route_name, json_build_array(last_dispatched_sequence,
      retry_attempt_count, extract(epoch FROM retry_next_attempt_at))), '{}')
  FROM bc_helpdesk.cross_context_route_cursors),
 (SELECT count(*) FROM bc_marketing.parked_deliveries),
 (SELECT count(*) FROM bc_knowledge_base.parked_deliveries),
 (SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'),
 (SELECT count(*) FROM pg_stat_activity WHERE datname = current_database()),
 (SELECT max(sequence) FROM bc_helpdesk.events WHERE event_type_name = 'CompanyExpired'),
 (SELECT max(sequence) FROM bc_helpdesk.events WHERE event_type_name = 'CompanyActivated'),
 (SELECT max(sequence) FROM bc_helpdesk.events WHERE event_type_name = 'TicketResolved')
"""
HEAD_OF = {"HelpdeskExpiryToTrialLapse": 6, "HelpdeskActivationToTrialConversion": 7,
           "HelpdeskResolutionToArticleSuggestion": 8}


def sample_once(server):
    out = psql(SAMPLE_SQL, DB).strip().split("|")
    cursors = json.loads(out[1])
    row = {"t": round(time.time(), 3), "head": int(out[0]), "parked_marketing": int(out[2]),
           "parked_kb": int(out[3]), "lock_waits": int(out[4]), "connections": int(out[5]),
           "rss_kb": server.rss_kb()}
    for route, idx in HEAD_OF.items():
        cur = cursors.get(route, [-1, 0, None])
        src_head = int(out[idx]) if out[idx] else -1
        row[f"{route}.cursor"] = cur[0]
        row[f"{route}.behind"] = max(0, src_head - cur[0])  # in helpdesk sequence numbers
        row[f"{route}.retry"] = cur[1]
    return row


def sampler(server):
    while not stop_all.is_set():
        t = time.time()
        try:
            samples.append(sample_once(server))
        except Exception as e:  # noqa: BLE001
            print("sample failed:", e, flush=True)
        time.sleep(max(0, 0.5 - (time.time() - t)))


# --- faults ---

class Fault:
    def __init__(self, spec, context):
        self.kind, _, self.arg = spec.partition(":")
        self.schema = f"bc_{context}"
        self.proc = None

    def start(self):
        s = self.schema
        if self.kind in ("outage", "flaky", "slow"):
            if self.kind == "outage":
                body = "RAISE EXCEPTION 'simulated outage'; "
            elif self.kind == "flaky":
                body = f"IF random() < {float(self.arg)} THEN RAISE EXCEPTION 'simulated flaky target'; END IF; "
            else:
                body = f"PERFORM pg_sleep({float(self.arg) / 1000}); "
            psql(f"CREATE FUNCTION {s}.bench_fault() RETURNS trigger LANGUAGE plpgsql AS "
                 f"$$ BEGIN {body}RETURN NEW; END $$; "
                 f"CREATE TRIGGER bench_fault BEFORE INSERT ON {s}.events "
                 f"FOR EACH ROW EXECUTE FUNCTION {s}.bench_fault();", DB)
        elif self.kind == "lock":
            self.proc = subprocess.Popen(
                [f"{PGBIN}/psql", "-h", PGHOST, "-p", PGPORT, "-U", PGUSER, "-d", DB, "-X", "-q"],
                stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, text=True)
            self.proc.stdin.write(f"BEGIN; SELECT next_value FROM {s}.sequence FOR UPDATE;\n")
            self.proc.stdin.flush()
        else:
            raise SystemExit(f"unknown fault {self.kind}")

    def stop(self):
        s = self.schema
        if self.kind == "lock":
            self.proc.stdin.write("COMMIT;\n\\q\n")
            self.proc.stdin.flush()
            self.proc.wait(timeout=30)
        else:
            psql(f"DROP TRIGGER bench_fault ON {s}.events; DROP FUNCTION {s}.bench_fault();", DB)


# --- GraphQL (redrive) ---

def graphql_jwt(log_path):
    lines = open(log_path).read().splitlines()
    for i, line in enumerate(lines):
        if line.startswith("GraphQL Role credential"):
            return lines[i + 1].strip()
    return None


def graphql(port, jwt, query):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=120)
    c.request("POST", "/graphql", json.dumps({"query": query}),
              {"authorization": f"Bearer {jwt}", "content-type": "application/json"})
    data = json.loads(c.getresponse().read())
    c.close()
    return data


def redrive(port, jwt):
    res = {}
    for ctx in ("marketing", "knowledge_base"):
        got = graphql(port, jwt, f'query {{ parkedDeliveries(boundedContext: "{ctx}") {{ id attemptCount error }} }}')
        parked = (got.get("data") or {}).get("parkedDeliveries") or []
        ok = failed = 0
        t0 = time.time()
        for p in parked:
            r = graphql(port, jwt, f'mutation {{ retryParkedDelivery(boundedContext: "{ctx}", id: "{p["id"]}") {{ id }} }}')
            if r.get("errors"):
                failed += 1
            else:
                ok += 1
        res[ctx] = {"parked": len(parked), "redriven": ok, "failed": failed,
                    "seconds": round(time.time() - t0, 2),
                    "errors_sample": sorted({p["error"][:120] for p in parked})[:3],
                    "query_errors": got.get("errors")}
    return res


# --- analysis ---

def fetch(schema, event_type, key, prefix):
    out = psql(f"SELECT sequence, extract(epoch FROM metadata_created_at), payload::jsonb->>'{key}' "
               f"FROM {schema}.events WHERE event_type_name = '{event_type}' "
               f"AND payload::jsonb->>'{key}' LIKE '{prefix}%' ORDER BY sequence", DB)
    rows = []
    for line in out.splitlines():
        seq, ts, k = line.split("|")
        rows.append((int(seq), float(ts), k))
    return rows


def analyse(phase_windows):
    res = {}
    pairs_out = []
    for route, (src, ctx, tgt, key) in ROUTES.items():
        sources = fetch("bc_helpdesk", src, key, PREFIX)
        targets = fetch(f"bc_{ctx}", tgt, key, PREFIX)
        by_key = {}
        for seq, ts, k in targets:
            by_key.setdefault(k, []).append((seq, ts))
        src_keys = {}
        for seq, ts, k in sources:
            src_keys.setdefault(k, []).append((seq, ts))
        dup = sum(max(0, len(v) - 1) for v in by_key.values())
        orphan = sum(1 for k in by_key if k not in src_keys)
        missing = []
        lags = []
        parked = set(psql(f"SELECT identifier FROM bc_{ctx}.parked_deliveries "
                          f"WHERE source LIKE '%{route}'", DB).split())
        for k, srcs in src_keys.items():
            if len(srcs) > 1:
                print(f"warning: {route}: {k} has {len(srcs)} source events", flush=True)
            tg = by_key.get(k)
            if not tg:
                missing.append(str(srcs[0][0]))
                continue
            lag = (tg[0][1] - srcs[0][1]) * 1000
            lags.append((srcs[0][1], lag))
            pairs_out.append((route, k, srcs[0][0], srcs[0][1], tg[0][0], tg[0][1], round(lag, 2)))
        per_phase = {}
        for name, t0, t1 in phase_windows:
            per_phase[name] = summary([lag for ts, lag in lags if t0 <= ts < t1])
        res[route] = {"sources": len(sources), "routed": len(targets) - dup, "duplicates": dup,
                      "orphans": orphan, "missing": len(missing),
                      "missing_and_parked": sum(1 for m in missing if m in parked),
                      "parked": len(parked), "all": summary([x for _, x in lags]),
                      "by_phase": per_phase}
    # Ordering: helpdesk says expire then activate; does marketing agree?
    lapsed = {k: s for s, _, k in fetch("bc_marketing", "TrialLapsed", "company_id", PREFIX)}
    conv = {k: s for s, _, k in fetch("bc_marketing", "TrialConverted", "company_id", PREFIX)}
    both = [k for k in lapsed if k in conv]
    inverted = [k for k in both if conv[k] < lapsed[k]]
    state = psql("SELECT state::jsonb->>'trial_status', count(*) FROM bc_marketing.projection_state "
                 f"WHERE projection_name = 'MarketingSignals' AND key LIKE '{PREFIX}%' "
                 "AND key NOT LIKE '%fg-%' GROUP BY 1", DB)
    res["ordering"] = {"companies_with_both": len(both), "inverted_in_marketing": len(inverted),
                       "marketing_signals_trial_status": dict(l.split("|") for l in state.splitlines())}
    return res, pairs_out


def wait_settled(server, t_limit):
    """Until every route is at its source head with no retry pending, for 3 samples."""
    ok = 0
    t0 = time.time()
    while time.time() - t0 < t_limit:
        s = sample_once(server)
        samples.append(s)
        if all(s[f"{r}.behind"] == 0 and s[f"{r}.retry"] == 0 for r in ROUTES):
            ok += 1
            if ok >= 3:
                return time.time() - t0
        else:
            ok = 0
        time.sleep(0.5)
    return None


def main():
    os.makedirs(args.out, exist_ok=True)
    dropdb(DB)
    if args.template == "-":
        createdb(DB)
    else:
        createdb(DB, args.template)
        psql("VACUUM ANALYZE", DB)
    load = open("/proc/loadavg").read().split()[:3]
    print(f"[{args.label}] loadavg before: {load}", flush=True)
    log_path = f"{args.out}/{args.label}-server.log"
    server = Server(args.binary, DB, args.port, log_path)
    marks["server_up"] = time.time()
    threads = [threading.Thread(target=sampler, args=(server,), daemon=True)]
    threads += [threading.Thread(target=prober, args=(k, server), daemon=True)
                for k in range(args.probe_threads)]
    threads += [threading.Thread(target=fg_worker, args=(w, server), daemon=True)
                for w in range(args.fg_workers)]
    for t in threads:
        t.start()
    time.sleep(args.warmup)

    fault = Fault(args.fault, args.fault_context) if args.fault else None
    fault_on = False
    windows = []
    probe_on.set()
    for name, secs, flags in phases():
        t0 = time.time()
        (fg_on.set if "load" in flags else fg_on.clear)()
        if fault and "fault" in flags and not fault_on:
            fault.start()
            fault_on = True
            marks.setdefault("fault_start", time.time())
        if fault and "fault" not in flags and fault_on:
            fault.stop()
            fault_on = False
            marks["fault_end"] = time.time()
        print(f"[{args.label}] phase {name} {secs:.0f}s {sorted(flags)} "
              f"load {open('/proc/loadavg').read().split()[0]}", flush=True)
        time.sleep(secs)
        windows.append((name, t0, time.time()))
    if fault_on:
        fault.stop()
        marks["fault_end"] = time.time()
    probe_on.clear()
    fg_on.clear()
    time.sleep(args.gap_ms / 1000 + 3)  # let started probes / tickets finish
    marks["traffic_end"] = time.time()
    stop_all.set()
    for t in threads[1:]:
        t.join(timeout=60)
    settled = wait_settled(server, args.settle_max)
    print(f"[{args.label}] routes settled {settled}s after traffic stopped", flush=True)
    result = {"label": args.label, "args": vars(args), "loadavg_before": load,
              "settle_after_traffic_s": settled, "marks": marks,
              "phases": [(n, round(a - marks['server_up'], 1), round(b - marks['server_up'], 1))
                         for n, a, b in windows]}
    result["routes"], pairs = analyse(windows)
    if args.redrive:
        jwt = graphql_jwt(log_path)
        result["redrive"] = redrive(server.port, jwt)
        time.sleep(3)
        result["routes_after_redrive"], pairs = analyse(windows)
    # Foreground / probe command latency per phase.
    cmd = {}
    for name, t0, t1 in windows:
        with lat_lock:
            rows = [r for r in latencies if t0 <= r[0] < t1]
        for kind in ("fg", "probe"):
            ok = [r[5] for r in rows if r[1] == kind and r[3] == 200]
            s = summary(ok)
            s["failures"] = sum(1 for r in rows if r[1] == kind and r[3] != 200)
            s["rejected"] = sum(1 for r in rows if r[1] == kind and r[3] == 200 and not r[4])
            cmd[f"{name}.{kind}"] = s
    result["commands"] = cmd
    result["probes"] = {"started": len(probe_log), "all_accepted": sum(1 for p in probe_log if p[4])}
    result["peak_rss_mb"] = round(server.hwm_kb() / 1024) if server.hwm_kb() else None
    # Recovery: time from fault end until each route is at its head with no retry.
    if "fault_end" in marks:
        rec = {}
        for r in ROUTES:
            after = [s for s in samples if s["t"] >= marks["fault_end"]]
            first = next((s["t"] for i, s in enumerate(after)
                          if all(x[f"{r}.behind"] <= 2 and x[f"{r}.retry"] == 0
                                 for x in after[i:i + 3])), None)
            rec[r] = round(first - marks["fault_end"], 1) if first else None
            during = [s for s in samples if marks["fault_start"] <= s["t"] <= marks["fault_end"]]
            rec[r + ".max_behind_during"] = max((s[f"{r}.behind"] for s in during), default=None)
            rec[r + ".max_retry"] = max((s[f"{r}.retry"] for s in samples), default=None)
        rec["parked_marketing_max"] = max(s["parked_marketing"] for s in samples)
        rec["parked_kb_max"] = max(s["parked_kb"] for s in samples)
        result["recovery"] = rec
    server.stop()
    with open(f"{args.out}/{args.label}.json", "w") as f:
        json.dump(result, f, indent=1)
    with open(f"{args.out}/{args.label}-samples.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(samples[0].keys()))
        w.writeheader()
        w.writerows(samples)
    with open(f"{args.out}/{args.label}-pairs.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["route", "key", "src_seq", "src_t", "tgt_seq", "tgt_t", "lag_ms"])
        w.writerows(pairs)
    with open(f"{args.out}/{args.label}-latency.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t", "kind", "command", "status", "accepted", "ms"])
        w.writerows(latencies)
    dropdb(DB)
    print(json.dumps({k: result[k] for k in ("routes", "recovery", "redrive") if k in result},
                     indent=1), flush=True)


if __name__ == "__main__":
    main()
