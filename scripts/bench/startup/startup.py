#!/usr/bin/env python3
"""Server startup time on a given history (issue #30).

    python3 startup.py <server> <template-db|-> <label> <out-dir> [options]

Copies the template (or starts from an empty database with `-`), then
starts the server `--starts` times and times each start:

- `build_start`: spawn to skilj's `migrate` (the server's own set-up:
  pools, demo roles, its local JWKS key pair).
- `patches_end`: the last per-context schema patch (sqlx notices).
- `build`: spawn to "reconciliation complete" (the rest of
  `Skilj::build()`: event-cache warm-up, reconciliation).
- `ready`: spawn to "listening" (adds the server's token minting).
- `first_ok`: spawn to the first accepted command.

While a start runs, Postgres is sampled every 100 ms: the server's
connections by state/wait event and query prefix, and how many backends
wait on a lock. Statements over 100 ms land in the cluster log
(`log_min_duration_statement`).

`--mode`:
- `cold` (default): nothing else running; each start is a fresh process
  after a clean SIGINT stop.
- `restart-load`: `--fg-workers` clients drive the ticket walk (create,
  assign, note, resolve) against the server; it is stopped and started
  again under that load. Measures downtime as seen by the clients.
- `peer-load`: a second instance (B) on the same database serves the
  clients throughout; instance A is started and stopped `--starts` times.
  Measures A's start under load and what A's start does to B's latency
  (§158: startup patches should no longer queue behind open reads).

Writes <label>.json (per-start phases, samples summary, client latency per
window) and <label>-<n>.log (each start's stdout with timestamps).
"""
import argparse
import http.client
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "deadline-drain"))
from common import (KEY_FILE, PGUSER, PGHOST, PGPORT, Client, createdb, dropdb, psql,  # noqa: E402
                    ticket_payload)

ap = argparse.ArgumentParser()
ap.add_argument("binary")
ap.add_argument("template")
ap.add_argument("label")
ap.add_argument("out")
ap.add_argument("--starts", type=int, default=3)
ap.add_argument("--mode", default="cold", choices=["cold", "restart-load", "peer-load"])
ap.add_argument("--fg-workers", type=int, default=8)
ap.add_argument("--fg-interval-ms", type=int, default=250)
ap.add_argument("--between", type=float, default=20, help="seconds between starts")
ap.add_argument("--port", type=int, default=18401)
args = ap.parse_args()
os.makedirs(args.out, exist_ok=True)
DB = f"su_{re.sub(r'[^a-z0-9]', '_', args.label.lower())}"
TOKEN = re.compile(r"^  (\w+): ([0-9a-f]{32}\.[0-9a-f]{64})$")
MARKS = {"build_start": '"_sqlx_migrations" already exists', "build": "server: reconciliation complete",
         "ready": "skilj-helpdesk listening on"}
# RUST_LOG adds sqlx's Postgres notices, nothing per request: `migrate`'s
# notice marks build()'s start and the last "already exists" notice the end
# of the per-context schema patches.
LOG_FILTER = "warn,sqlx::postgres::notice=info"


def load():
    return open("/proc/loadavg").read().split()[:3]


class Proc:
    """One server process with every stdout line timestamped."""

    def __init__(self, port, log_path):
        env = dict(os.environ)
        env.update({"DATABASE_URL": f"postgres://{PGUSER}@{PGHOST}:{PGPORT}/{DB}",
                    "ENCRYPTION_KEY_FILE": KEY_FILE, "PORT": str(port),
                    "RUST_LOG": env.get("RUST_LOG", LOG_FILTER)})
        self.port, self.marks, self.tokens = port, {}, {}
        self.log = open(log_path, "w")
        self.t0 = time.time()
        self.proc = subprocess.Popen([args.binary], env=env, stdout=subprocess.PIPE,
                                     stderr=subprocess.STDOUT, text=True, bufsize=1)
        self.ready = threading.Event()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        section = None
        for line in self.proc.stdout:
            t = time.time() - self.t0
            self.log.write(f"{t:9.3f} {line}")
            if "first_line" not in self.marks:
                self.marks["first_line"] = t
            if "already exists, skipping" in line and "build" not in self.marks:
                self.marks["patches_end"] = t
            for name, text in MARKS.items():
                if name not in self.marks and text in line:
                    self.marks[name] = t
            if line.startswith("command tokens"):
                section = "cmd"
            elif line and not line.startswith(" "):
                section = None
            m = TOKEN.match(line.rstrip("\n"))
            if section == "cmd" and m:
                self.tokens.setdefault(m.group(1), m.group(2))
            if "ready" in self.marks and "ResolveTicket" in self.tokens:
                self.ready.set()
        self.log.flush()

    def wait_ready(self, timeout=900):
        if not self.ready.wait(timeout):
            raise RuntimeError("server never became ready")

    def rss_mb(self, field="VmHWM"):
        try:
            for line in open(f"/proc/{self.proc.pid}/status"):
                if line.startswith(field + ":"):
                    return int(line.split()[1]) // 1024
        except OSError:
            return None

    def stop(self):
        t = time.time()
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGINT)
            try:
                self.proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()
        return time.time() - t


# --- Postgres sampling while a start runs ---

SAMPLE_SQL = f"""
SELECT coalesce(wait_event_type, 'cpu') || ':' || coalesce(wait_event, state) || '|' ||
       left(regexp_replace(query, '\\s+', ' ', 'g'), 60)
FROM pg_stat_activity WHERE datname = '{DB}' AND state <> 'idle' AND pid <> pg_backend_pid()
"""


class Sampler:
    def __init__(self):
        self.stop_ev, self.counts, self.lock_waits_max, self.n = threading.Event(), {}, 0, 0
        threading.Thread(target=self._run, daemon=True).start()

    def _run(self):
        while not self.stop_ev.is_set():
            try:
                rows = psql(SAMPLE_SQL, "postgres").splitlines()
                waits = int(psql(f"SELECT count(*) FROM pg_stat_activity WHERE datname = '{DB}' "
                                 "AND wait_event_type = 'Lock'", "postgres") or 0)
            except Exception:
                rows, waits = [], 0
            self.n += 1
            self.lock_waits_max = max(self.lock_waits_max, waits)
            for r in rows:
                self.counts[r] = self.counts.get(r, 0) + 1
            time.sleep(0.1)

    def finish(self):
        self.stop_ev.set()
        top = sorted(self.counts.items(), key=lambda kv: -kv[1])[:12]
        return {"samples": self.n, "lock_waits_max": self.lock_waits_max,
                "top_active": [[q, c] for q, c in top]}


# --- client load ---

COMPANIES = []
lat_lock = threading.Lock()
calls = []  # (t, port, status, accepted, ms)
stop_load = threading.Event()


def worker(w, target):
    """`target()` gives the (port, tokens) to use now; follows restarts."""
    n = 0
    client, cur = None, None
    while not stop_load.is_set():
        port, tokens = target()
        if not tokens:
            time.sleep(0.05)
            continue
        if (port, id(tokens)) != cur:
            client, cur = Client(port, tokens), (port, id(tokens))
        tid = f"su-{args.label}-{w}-{n}"
        n += 1
        steps = [("CreateTicket", ticket_payload(tid, COMPANIES[n % len(COMPANIES)], f"su-cust-{w}")),
                 ("AssignTicket", {"ticket_id": tid, "staff_id": f"seed-staff-{w % 5}"}),
                 ("AddInternalNote", {"ticket_id": tid, "staff_id": f"seed-staff-{w % 5}",
                                      "note": "n"}),
                 ("ResolveTicket", {"ticket_id": tid})]
        for command, payload in steps:
            t = time.time()
            try:
                status, accepted, ms = client.trigger(command, payload)
            except (OSError, http.client.HTTPException):
                status, accepted, ms = 0, False, (time.time() - t) * 1000
                client = Client(port, tokens)
            with lat_lock:
                calls.append((t, port, status, accepted, ms))
            time.sleep(args.fg_interval_ms / 1000)


def pct(xs, p):
    xs = sorted(xs)
    return round(xs[min(len(xs) - 1, int(p / 100 * len(xs)))], 1) if xs else None


def window(t0, t1, port=None):
    with lat_lock:
        sel = [c for c in calls if t0 <= c[0] < t1 and (port is None or c[1] == port)]
    ok = [c[4] for c in sel if c[2] == 200]
    statuses = {}
    for c in sel:
        k = f"{c[2]}/{'accepted' if c[3] else 'rejected'}"
        statuses[k] = statuses.get(k, 0) + 1
    return {"n": len(sel), "ok": len(ok), "failed": len(sel) - len(ok), "statuses": statuses,
            "p50": pct(ok, 50), "p99": pct(ok, 99), "max": round(max(ok), 1) if ok else None}


def start_once(i, port):
    sampler = Sampler()
    p = Proc(port, f"{args.out}/{args.label}-{i}.log")
    p.wait_ready()
    rec = {"start": i, "t_spawn": p.t0, "load_before": load(), **{k: round(v, 3) for k, v in p.marks.items()}}
    return p, sampler, rec


def main():
    dropdb(DB)
    if args.template == "-":
        createdb(DB)
    else:
        createdb(DB, args.template)
    events = {s: psql(f"SELECT count(*) FROM bc_{s}.events", DB)
              for s in ("helpdesk", "activity", "marketing", "knowledge_base")} \
        if args.template != "-" else {}
    if args.template == "-":
        COMPANIES.append("su-co")  # doesn't exist: commands answer, rejected
    else:
        COMPANIES.extend(psql("SELECT payload::jsonb->>'company_id' FROM bc_helpdesk.events "
                              "WHERE event_type_name = 'CompanySignedUp'", DB).split())
    res = {"label": args.label, "args": vars(args), "events": events, "starts": []}
    workers = []

    if args.mode == "peer-load":
        peer = Proc(args.port + 1, f"{args.out}/{args.label}-peer.log")
        peer.wait_ready()
        workers = [threading.Thread(target=worker, args=(w, lambda: (peer.port, peer.tokens)))
                   for w in range(args.fg_workers)]
        for t in workers:
            t.start()
        time.sleep(args.between)

    current = {"p": None}

    def target():
        p = current["p"]
        return (p.port, p.tokens) if p and p.ready.is_set() else (args.port, None)

    for i in range(args.starts):
        if args.mode == "restart-load" and current["p"] is None:
            p, sampler, rec = start_once(i, args.port)
            sampler.finish()
            current["p"] = p
            workers = [threading.Thread(target=worker, args=(w, target))
                       for w in range(args.fg_workers)]
            for t in workers:
                t.start()
            time.sleep(args.between)
        t_stop = time.time()
        if args.mode == "restart-load":
            stop_s = current["p"].stop()
        p, sampler, rec = start_once(i, args.port)
        # first accepted command after spawn, from the clients or our own probe
        if args.mode != "restart-load":
            c = Client(p.port, p.tokens)
            tid = f"su-probe-{args.label}-{i}"
            c.trigger("CreateTicket", ticket_payload(tid, COMPANIES[0]))
            rec["first_ok"] = round(time.time() - p.t0, 3)
        else:
            current["p"] = p
            deadline = time.time() + 120
            while time.time() < deadline:
                with lat_lock:
                    oks = [c[0] for c in calls if c[0] >= p.t0 and c[2] == 200 and c[3]]
                if oks:
                    rec["first_ok"] = round(min(oks) - p.t0, 3)
                    break
                time.sleep(0.05)
            rec["stop_s"] = round(stop_s, 3)
            with lat_lock:
                before = [c[0] + c[4] / 1000 for c in calls if c[0] < t_stop and c[2] == 200]
            rec["client_gap_s"] = round(p.t0 + rec.get("first_ok", 0) - max(before), 3) \
                if before else None
        rec["rss_mb_ready"] = p.rss_mb("VmRSS")
        time.sleep(min(args.between, 10))
        rec["postgres"] = sampler.finish()
        rec["rss_mb_peak"] = p.rss_mb("VmHWM")
        if args.mode == "peer-load":
            t_ready = p.t0 + rec["ready"]
            rec["peer_before"] = window(p.t0 - args.between + 2, p.t0)
            rec["peer_during_start"] = window(p.t0, t_ready)
            rec["peer_after"] = window(t_ready, t_ready + 10)
        res["starts"].append(rec)
        print(json.dumps({k: v for k, v in rec.items() if k != "postgres"}), flush=True)
        if args.mode != "restart-load":
            p.stop()
            time.sleep(args.between)

    stop_load.set()
    for t in workers:
        t.join()
    if current["p"]:
        current["p"].stop()
    if args.mode == "peer-load":
        peer.stop()
    if workers:
        res["clients_total"] = window(0, time.time() + 1)
    json.dump(res, open(f"{args.out}/{args.label}.json", "w"), indent=1)
    dropdb(DB)


if __name__ == "__main__":
    main()
