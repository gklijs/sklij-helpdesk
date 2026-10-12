#!/usr/bin/env python3
"""One deadline-drain run (issue #28). See README.md in this directory.

usage: drain.py <server-binary> <template-db> <label> <out-dir> [options]

  --variant autoclose|sla|none  which pending deadlines are moved to come due
                                together (none: a control run, nothing due)
  --due-in S          seconds from server start until they come due (180);
                      the "before" window is the last 60s of it, clear of the
                      server's own startup catch-up
  --fg-workers N      foreground workers, each one keep-alive connection (8)
  --fg-interval-ms M  each worker's pacing (250 -> 8 workers ~32 commands/s)
  --after S           seconds of foreground load kept after the drain (60)
  --max-drain S       give up waiting for the drain after S seconds (1800);
                      with --variant none, the length of the control window
  --retention         afterwards, age every resolved deadline past the 30-day
                      retention and run skilj's own retention statement in
                      the same batches as its `retention_tick`, foreground
                      load still running

Copies <template-db> to a fresh database, moves every pending deadline of
the variant's schedules to `now() + due-in`, starts the server, and drives
a steady foreground load (create -> assign -> note -> note -> resolve, one
ticket at a time per worker, own companies) while a sampler records the
deadline table, the sequence head, async projection lag, connection waits
and server RSS every 0.5s. Writes <label>.json (summary), <label>-samples.csv
and <label>-latency.csv into <out-dir>.
"""
import argparse
import csv
import json
import re
import subprocess
import threading
import time

from common import (PGBIN, PGHOST, PGPORT, PGUSER, SCHEMA, Client, Server,
                    createdb, dropdb, psql, summary, ticket_payload)

SCHEDULES = {
    "autoclose": ["ScheduleTicketAutoClose"],
    "sla": ["ScheduleFirstResponseSla", "ScheduleResolutionSla"],
    "none": ["ScheduleTicketAutoClose"],
}
# skilj 0.0.10 `delete_expired_deadlines` + `retention_tick`, verbatim
# statement, same batch size (IDEMPOTENCY_KEY_CLEANUP_BATCH = 10_000).
RETENTION_SQL = (
    f"DELETE FROM {SCHEMA}.deadlines WHERE ctid IN ( "
    f"SELECT ctid FROM {SCHEMA}.deadlines "
    f"WHERE status NOT IN ('pending', 'firing') AND resolved_at < now() - interval '30 days' "
    f"LIMIT 10000)"
)

ap = argparse.ArgumentParser()
ap.add_argument("binary")
ap.add_argument("template")
ap.add_argument("label")
ap.add_argument("out")
ap.add_argument("--variant", default="autoclose", choices=SCHEDULES)
ap.add_argument("--due-in", type=float, default=180)
ap.add_argument("--fg-workers", type=int, default=8)
ap.add_argument("--fg-interval-ms", type=int, default=250)
ap.add_argument("--after", type=float, default=60)
ap.add_argument("--max-drain", type=float, default=1800)
ap.add_argument("--retention", action="store_true")
ap.add_argument("--port", type=int, default=18192)
args = ap.parse_args()

DB = f"dd_run_{args.label}".replace("-", "_")
names = SCHEDULES[args.variant]
name_list = ", ".join(f"'{n}'" for n in names)
stop_fg = threading.Event()
stop_sampler = threading.Event()
latencies = []  # (wall_time, command, status, accepted, ms)
lat_lock = threading.Lock()
samples = []
events = {}


def fg_worker(w, server):
    client = Client(server.port, server.tokens)
    company = f"fg-{args.label}-{w}"
    client.trigger("SignUpCompany", {"company_id": company, "name": company,
                                     "contact_email": "fg@x.example"})
    interval = args.fg_interval_ms / 1000
    n = 0
    nxt = time.time() + interval * w / args.fg_workers
    while not stop_fg.is_set():
        tid = f"fg-{args.label}-{w}-{n}"
        n += 1
        steps = [
            ("CreateTicket", ticket_payload(tid, company)),
            ("AssignTicket", {"ticket_id": tid, "staff_id": "fg-staff"}),
            ("AddInternalNote", {"ticket_id": tid, "staff_id": "fg-staff", "note": "a"}),
            ("AddInternalNote", {"ticket_id": tid, "staff_id": "fg-staff", "note": "b"}),
            ("ResolveTicket", {"ticket_id": tid}),
        ]
        for command, payload in steps:
            if stop_fg.is_set():
                return
            delay = nxt - time.time()
            if delay > 0:
                time.sleep(delay)
            nxt += interval
            t = time.time()
            try:
                status, accepted, ms = client.trigger(command, payload)
            except Exception:  # noqa: BLE001 - counted as a transport failure
                status, accepted, ms = -1, False, (time.time() - t) * 1000
            with lat_lock:
                latencies.append((t, command, status, accepted, ms))


SAMPLE_SQL = """
SELECT
 (SELECT next_value FROM {schema}.sequence),
 (SELECT count(*) FILTER (WHERE status = 'pending') || ',' || count(*) FILTER (WHERE status = 'firing')
         || ',' || count(*) FILTER (WHERE status = 'fired')
  FROM {schema}.deadlines WHERE schedule_name IN ({names}) AND fire_at <= to_timestamp({due})),
 (SELECT least(
     (SELECT min(caught_up_to) FROM {schema}.projections WHERE NOT sync),
     (SELECT min(caught_up_to) FROM {schema}.projection_partition_progress))),
 (SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND state = 'active'),
 (SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'),
 (SELECT count(*) FROM {schema}.deadlines)
"""


def sampler(server):
    while not stop_sampler.is_set():
        t = time.time()
        try:
            sql = SAMPLE_SQL.format(schema=SCHEMA, names=name_list, due=events.get("t_due", 0) + 1)
            out = psql(sql, DB).strip().split("|")
            head = int(out[0])
            pending, firing, fired = (int(x) for x in out[1].split(","))
            min_async = int(out[2]) if out[2] else head
            samples.append({"t": round(t, 3), "head": head, "pending": pending, "firing": firing,
                            "fired": fired, "async_lag": head - min_async,
                            "active": int(out[3]), "lock_waits": int(out[4]),
                            "deadline_rows": int(out[5]), "rss_kb": server.rss_kb()})
        except Exception as e:  # noqa: BLE001
            print("sample failed:", e, flush=True)
        time.sleep(max(0, 0.5 - (time.time() - t)))


def run_retention():
    """Replays skilj's retention_tick for the helpdesk context: batches of
    10,000 until one deletes fewer. Each batch timed by psql itself."""
    batches = []
    while True:
        out = subprocess.run(
            [f"{PGBIN}/psql", "-h", PGHOST, "-p", PGPORT, "-U", PGUSER, "-d", DB, "-X",
             "-c", "\\timing on", "-c", RETENTION_SQL],
            capture_output=True, text=True, check=True).stdout
        deleted = int(re.search(r"DELETE (\d+)", out).group(1))
        ms = float(re.search(r"Time: ([\d.]+) ms", out).group(1))
        batches.append({"t": time.time(), "deleted": deleted, "ms": ms})
        if deleted < 10000:
            return batches


def window(t0, t1, ok_only=True):
    with lat_lock:
        rows = [r for r in latencies if t0 <= r[0] < t1]
    ok = [r[4] for r in rows if r[2] == 200]
    by_cmd = {}
    for r in rows:
        if r[2] == 200:
            by_cmd.setdefault(r[1], []).append(r[4])
    res = summary(ok)
    res["seconds"] = round(t1 - t0, 1)
    res["rate"] = round(len(rows) / (t1 - t0), 2) if t1 > t0 else None
    res["failures"] = sum(1 for r in rows if r[2] != 200)
    res["rejected"] = sum(1 for r in rows if r[2] == 200 and not r[3])
    res["by_command_p50_p99"] = {k: [summary(v)["p50"], summary(v)["p99"]] for k, v in by_cmd.items()}
    return res


def main():
    dropdb(DB)
    createdb(DB, args.template)
    psql("VACUUM ANALYZE", DB)
    load = open("/proc/loadavg").read().split()[:3]
    print(f"[{args.label}] loadavg before: {load}", flush=True)

    server = Server(args.binary, DB, args.port, f"{args.out}/{args.label}-server.log")
    # The server is up and idle; now set the due time, so the "before"
    # window is a warm server under foreground load only.
    t_due = time.time() + args.due_in
    moved = 0
    if args.variant != "none":
        moved = int(psql(
            f"WITH m AS (UPDATE {SCHEMA}.deadlines SET fire_at = to_timestamp({t_due}) "
            f"WHERE status = 'pending' AND schedule_name IN ({name_list}) RETURNING 1) "
            f"SELECT count(*) FROM m", DB).strip())
    print(f"[{args.label}] {moved} deadlines due at +{args.due_in:.0f}s", flush=True)
    events["t_due"] = t_due
    events["moved"] = moved

    smp = threading.Thread(target=sampler, args=(server,))
    smp.start()
    t_fg = time.time()
    workers = [threading.Thread(target=fg_worker, args=(w, server)) for w in range(args.fg_workers)]
    for w in workers:
        w.start()

    # Wait for the drain.
    t_drained = None
    while time.time() < t_due + args.max_drain:
        time.sleep(0.5)
        if args.variant == "none":
            if time.time() > t_due + args.max_drain - 2:
                t_drained = time.time()
                break
            continue
        if time.time() > t_due and samples and samples[-1]["pending"] + samples[-1]["firing"] == 0:
            t_drained = samples[-1]["t"]
            break
        if samples and int(time.time()) % 30 == 0:
            s = samples[-1]
            print(f"[{args.label}] +{time.time() - t_due:.0f}s pending={s['pending']} "
                  f"fired={s['fired']} lag={s['async_lag']} rss={s['rss_kb']}", flush=True)
    events["t_drained"] = t_drained
    time.sleep(args.after)
    t_after_end = time.time()

    retention = None
    if args.retention:
        aged = psql(
            f"WITH a AS (UPDATE {SCHEMA}.deadlines SET resolved_at = resolved_at - interval '31 days' "
            f"WHERE status NOT IN ('pending', 'firing') RETURNING 1) SELECT count(*) FROM a", DB).strip()
        rows_before = int(psql(f"SELECT count(*) FROM {SCHEMA}.deadlines", DB).strip())
        print(f"[{args.label}] aged {aged} resolved deadlines; settling 30s", flush=True)
        time.sleep(30)
        t_ret0 = time.time()
        batches = run_retention()
        t_ret1 = time.time()
        time.sleep(30)
        t_ret_after = time.time()
        retention = {
            "aged": int(aged), "rows_before": rows_before,
            "rows_after": int(psql(f"SELECT count(*) FROM {SCHEMA}.deadlines", DB).strip()),
            "wall_s": round(t_ret1 - t_ret0, 2), "batches": batches,
            "fg_before": window(t_ret0 - 30, t_ret0),
            "fg_during": window(t_ret0, max(t_ret1, t_ret0 + 1)),
            "fg_after": window(t_ret1, t_ret_after),
        }
        print(f"[{args.label}] retention: {retention['wall_s']}s, "
              f"{[(b['deleted'], b['ms']) for b in batches]}", flush=True)
        t_after_end = t_ret_after

    stop_fg.set()
    for w in workers:
        w.join()
    stop_sampler.set()
    smp.join()
    hwm = server.hwm_kb()
    server.stop()

    t_start = t_fg + 10
    result = {
        "label": args.label, "variant": args.variant, "template": args.template,
        "fg": {"workers": args.fg_workers, "interval_ms": args.fg_interval_ms},
        "loadavg_before": load, "moved": moved, "t_due": t_due, "t_drained": t_drained, "peak_rss_mb": round(hwm / 1024) if hwm else None,
    }
    if t_drained:
        before = window(t_due - 60, t_due)
        during = window(t_due, t_drained)
        after = window(t_drained, t_drained + args.after)
        upto = [s for s in samples if s["t"] <= t_drained + 0.01]
        due = [s for s in upto if s["t"] >= t_due]
        first_fire = next((s["t"] for s in due if s["fired"] > 0), None)
        fired_total = upto[-1]["fired"]
        result.update({
            "drain_s": round(t_drained - t_due, 1),
            "first_fire_after_s": round(first_fire - t_due, 2) if first_fire else None,
            "fired": fired_total,
            "fire_rate_per_s": round(moved / (t_drained - t_due), 1) if moved else None,
            "head_before": next((s["head"] for s in samples if s["t"] >= t_due), None),
            "head_after": upto[-1]["head"],
            "max_async_lag": max(s["async_lag"] for s in upto),
            "async_lag_at_drained": next((s["async_lag"] for s in samples if s["t"] >= t_drained), None),
            "max_lock_waits": max(s["lock_waits"] for s in upto),
            "latency_before": before, "latency_during": during, "latency_after": after,
        })
    else:
        result["drain_s"] = None
    result["retention"] = retention
    with open(f"{args.out}/{args.label}.json", "w") as f:
        json.dump(result, f, indent=1)
    with open(f"{args.out}/{args.label}-samples.csv", "w", newline="") as f:
        wr = csv.DictWriter(f, fieldnames=list(samples[0].keys()))
        wr.writeheader()
        wr.writerows(samples)
    with open(f"{args.out}/{args.label}-latency.csv", "w", newline="") as f:
        wr = csv.writer(f)
        wr.writerow(["t", "command", "status", "accepted", "ms"])
        wr.writerows(latencies)
    dropdb(DB)
    print(json.dumps({k: v for k, v in result.items() if k != "retention"}, indent=1))
    if retention:
        print(json.dumps({k: v for k, v in retention.items() if k != "batches"}, indent=1))


if __name__ == "__main__":
    main()
