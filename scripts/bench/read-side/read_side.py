#!/usr/bin/env python3
"""Read-side benchmarks (issue #31).

Measures the four things that issue names against one live server:

  * ``queryEvents`` paging over a long history, with the server's
    ``max_events_per_read`` cap (skilj's default is 1000);
  * catch-up rate of the real ``alerter`` and ``engagement-watcher``
    binaries replaying a backlog from their own fresh event-token cursor;
  * GraphQL projection-query latency while ticket writes are running;
  * fan-out latency to N concurrent GraphQL websocket subscribers.

Only Python's standard library is used for the driver.  The websocket
fan-out helper is ``fanout.js`` (Node's already-installed ``ws`` module);
everything else talks to the same REST/GraphQL surface the benchmark
suite's ``deadline-drain/common.py`` helpers use.

    python3 read_side.py <server-bin> <alerter-bin> <watcher-bin> <out-dir> \
        [--events 5000] [--load-seconds 45] [--fanout 1,8,32,64]

Writes ``read-side.json`` plus per-process logs into ``<out-dir>``.  The
report is ``docs/read-side-report-<date>.md``.
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
from common import Client, Server, createdb, dropdb, psql, summary, ticket_payload  # noqa: E402

PORT = 18301
DB = "rs_readside"
HELPDESK = "bc_helpdesk"
ACTIVITY = "bc_activity"

ap = argparse.ArgumentParser()
ap.add_argument("server")
ap.add_argument("alerter")
ap.add_argument("watcher")
ap.add_argument("out")
ap.add_argument("--events", type=int, default=5000,
                help="tickets in the long history (default 5000)")
ap.add_argument("--load-seconds", type=float, default=45)
ap.add_argument("--fanout", default="1,8,32,64")
ap.add_argument("--fanout-events", type=int, default=3)
ap.add_argument("--port", type=int, default=PORT)
args = ap.parse_args()

os.makedirs(args.out, exist_ok=True)
stop = threading.Event()


def say(msg):
    print(msg, flush=True)


def graphql(jwt, query, variables=None):
    body = json.dumps({"query": query, "variables": variables or {}})
    conn = http.client.HTTPConnection("127.0.0.1", args.port, timeout=120)
    t0 = time.perf_counter()
    conn.request("POST", "/graphql", body,
                 {"authorization": f"Bearer {jwt}", "content-type": "application/json"})
    resp = conn.getresponse()
    raw = resp.read()
    ms = (time.perf_counter() - t0) * 1000
    conn.close()
    data = json.loads(raw)
    if resp.status != 200 or data.get("errors"):
        raise RuntimeError(f"graphql http {resp.status}: {data}")
    return data["data"], ms


def parse_exports(log_path):
    """The ready-to-paste ``  export NAME=value`` block server.rs prints."""
    out = {}
    pat = re.compile(r"^  export ([A-Z0-9_]+)=(.*)$")
    for line in open(log_path, errors="replace"):
        m = pat.match(line.rstrip("\n"))
        if m:
            out[m.group(1)] = m.group(2)
    return out


def parse_jwt(log_path):
    """server.rs prints the local-shortcut admin JWT as a bare `  <jwt>`."""
    for line in open(log_path, errors="replace"):
        value = line.strip()
        if value.count(".") == 2 and len(value) > 100 and " " not in value:
            return value
    raise RuntimeError("no JWT in " + log_path)


def parse_command_token(log_path, name):
    pat = re.compile(rf"^  {re.escape(name)}: (\S+)$")
    for line in open(log_path, errors="replace"):
        m = pat.match(line.rstrip("\n"))
        if m:
            return m.group(1)
    raise RuntimeError(f"no {name} command token in {log_path}")


def wait_event_count(schema, event_type, wanted, timeout=600):
    deadline = time.time() + timeout
    while time.time() < deadline:
        n = int(psql(
            f"SELECT count(*) FROM {schema}.events WHERE event_type_name = '{event_type}'",
            DB).strip() or "0")
        if n >= wanted:
            return n
        time.sleep(0.25)
    raise RuntimeError(f"{schema}.{event_type} never reached {wanted}")


def run_workers(count, fn):
    """Run `fn(worker_index)` on `count` threads and re-raise the first error."""
    errors = []

    def wrapped(i):
        try:
            fn(i)
        except BaseException as exc:  # noqa: BLE001 - report from the main thread
            errors.append(exc)

    threads = [threading.Thread(target=wrapped, args=(i,), daemon=True)
               for i in range(count)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    if errors:
        raise errors[0]


def seed_tickets(company_id, count, workers, prefix, priority="low"):
    per_worker = (count + workers - 1) // workers

    def work(worker):
        client = Client(args.port, server.tokens)
        start = worker * per_worker
        end = min(count, start + per_worker)
        for i in range(start, end):
            client.trigger("CreateTicket",
                           ticket_payload(f"{prefix}-{i}", company_id, requester="bench-customer"))

    t0 = time.perf_counter()
    run_workers(workers, work)
    elapsed = time.perf_counter() - t0
    say(f"seeded {count} tickets for {company_id} in {elapsed:.1f}s "
        f"({count / elapsed:.1f}/s)")
    return elapsed


def seed_activity(count, workers):
    token = parse_command_token(server.log_path, "RecordDailyActivity")

    def work(worker):
        client = Client(args.port, {"RecordDailyActivity": token})
        per_worker = (count + workers - 1) // workers
        start = worker * per_worker
        end = min(count, start + per_worker)
        for i in range(start, end):
            client.trigger("RecordDailyActivity", {
                "company_id": f"rs-act-{i}",
                "person_kind": "customer",
                "person_subject": f"rs-person-{i}",
                "day": "2026-01-01T00:00:00Z",
            })

    t0 = time.perf_counter()
    run_workers(workers, work)
    elapsed = time.perf_counter() - t0
    say(f"seeded {count} DailyActivityRecorded events in {elapsed:.1f}s "
        f"({count / elapsed:.1f}/s)")
    return elapsed


def measure_query_events(jwt, bounded_context, event_type, expected):
    query = ("query Paging($bc: String!, $types: [String!]!, $after: Int) { "
             "queryEvents(boundedContext: $bc, eventTypes: $types, afterSequence: $after) "
             "{ sequence payload } }")
    pages, total, after = [], 0, 0
    t0 = time.perf_counter()
    while True:
        data, ms = graphql(jwt, query,
                           {"bc": bounded_context, "types": [event_type], "after": after})
        rows = data["queryEvents"]
        pages.append({"n": len(rows), "ms": round(ms, 2),
                      "first_sequence": rows[0]["sequence"] if rows else None,
                      "last_sequence": rows[-1]["sequence"] if rows else None})
        total += len(rows)
        if not rows:
            break
        after = rows[-1]["sequence"]
        if total >= expected:
            break
    elapsed = time.perf_counter() - t0
    return {
        "bounded_context": bounded_context,
        "event_type": event_type,
        "events": total,
        "expected": expected,
        "pages": pages,
        "page_cap": max((p["n"] for p in pages), default=0),
        "seconds": round(elapsed, 3),
        "events_per_s": round(total / elapsed, 1) if elapsed else None,
        "page_ms": summary([p["ms"] for p in pages]),
    }


def start_binary(binary, env, log_path):
    log = open(log_path, "w")
    full_env = dict(os.environ)
    full_env.update(env)
    full_env.pop("OTEL_EXPORTER_OTLP_ENDPOINT", None)
    proc = subprocess.Popen([binary], env=full_env, stdout=log,
                            stderr=subprocess.STDOUT)
    return proc, log


def stop_binary(proc, log):
    if proc.poll() is None:
        proc.send_signal(signal.SIGINT)
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    log.close()


def wait_state(path, key, wanted, timeout=300):
    deadline = time.time() + timeout
    last = -1
    while time.time() < deadline:
        try:
            with open(path) as f:
                state = json.load(f)
            n = len(state.get(key, {}))
            if n != last:
                last = n
            if n >= wanted:
                return n
        except (OSError, ValueError):
            pass
        time.sleep(0.05)
    raise RuntimeError(f"{path} never reached {wanted} {key} entries (last {last})")


def catch_up(label, binary, env, state_path, state_key, wanted):
    try:
        os.unlink(state_path)
    except FileNotFoundError:
        pass
    log_path = os.path.join(args.out, f"{label}.log")
    t0 = time.perf_counter()
    proc, log = start_binary(binary, env, log_path)
    try:
        wait_state(state_path, state_key, wanted)
        elapsed = time.perf_counter() - t0
        return {"binary": label, "events": wanted, "seconds": round(elapsed, 3),
                "events_per_s": round(wanted / elapsed, 1) if elapsed else None,
                "log": log_path}
    finally:
        stop_binary(proc, log)


def projection_query(jwt, kind, company, ticket):
    if kind == "TicketSummary":
        q = ("query P($bc: String!, $key: String!) { "
             "projection(boundedContext: $bc, name: \"TicketSummary\", key: $key) "
             "{ ... on helpdesk_TicketSummary { status priority } } }")
        variables = {"bc": "helpdesk", "key": ticket}
    else:
        q = ("query P($bc: String!, $key: String!) { "
             "projection(boundedContext: $bc, name: \"CompanyActiveTickets\", key: $key) "
             "{ ... on helpdesk_CompanyActiveTickets { tickets } } }")
        variables = {"bc": "helpdesk", "key": company}
    data, ms = graphql(jwt, q, variables)
    return data, ms


def projection_load(jwt, company, ticket):
    idle = {"TicketSummary": [], "CompanyActiveTickets": []}
    for kind in idle:
        for _ in range(10):
            _, ms = projection_query(jwt, kind, company, ticket)
            idle[kind].append(ms)

    stop.clear()
    lat = {"TicketSummary": [], "CompanyActiveTickets": []}
    errors = []
    writes = {"accepted": 0, "rejected": 0}
    lock = threading.Lock()

    def query_worker(i):
        kind = "TicketSummary" if i % 2 == 0 else "CompanyActiveTickets"
        while not stop.is_set():
            try:
                _, ms = projection_query(jwt, kind, company, ticket)
                with lock:
                    lat[kind].append(ms)
            except Exception as exc:  # noqa: BLE001 - counted below
                with lock:
                    errors.append(str(exc))
            time.sleep(0.01)

    def write_worker(i):
        client = Client(args.port, server.tokens)
        n = 0
        while not stop.is_set():
            ticket_id = f"rs-load-{i}-{n}"
            status, accepted, _ = client.trigger(
                "CreateTicket", ticket_payload(ticket_id, company, requester="bench-customer"))
            with lock:
                writes["accepted" if accepted else "rejected"] += 1
            if accepted:
                client.trigger("AssignTicket", {"ticket_id": ticket_id,
                                                "staff_id": f"staff-{i}"})
            n += 1

    qthreads = [threading.Thread(target=query_worker, args=(i,), daemon=True)
                for i in range(4)]
    wthreads = [threading.Thread(target=write_worker, args=(i,), daemon=True)
                for i in range(6)]
    for t in qthreads + wthreads:
        t.start()
    time.sleep(args.load_seconds)
    stop.set()
    for t in qthreads + wthreads:
        t.join()

    return {
        "seconds": args.load_seconds,
        "idle_ms": {k: summary(v) for k, v in idle.items()},
        "load_ms": {k: summary(v) for k, v in lat.items()},
        "errors": len(errors),
        "write_commands": writes,
        "write_per_s": round(sum(writes.values()) / args.load_seconds, 1),
        "sample_errors": errors[:3],
    }


def fanout(jwt, token, company, clients):
    latest_ticket = int(psql(
        "SELECT max(sequence) FROM bc_helpdesk.events WHERE event_type_name = 'TicketCreated'",
        DB).strip() or "0")
    cmd = ["node", os.path.join(os.path.dirname(__file__), "fanout.js"),
           "--port", str(args.port), "--jwt", jwt, "--token", token,
           "--company", company, "--clients", str(clients),
           "--events", str(args.fanout_events),
           "--from-sequence", str(max(0, latest_ticket - 1))]
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    for line in out.stdout.splitlines():
        if line.startswith("FANOUT "):
            return json.loads(line[len("FANOUT "):])
    raise RuntimeError(f"fanout.js failed ({out.returncode}): {out.stdout}\n{out.stderr}")


# --- campaign -------------------------------------------------------------

if __name__ == "__main__":
    dropdb(DB)
    createdb(DB)
    say(f"database {DB} created; starting server on {args.port}")
    server = Server(args.server, DB, args.port, os.path.join(args.out, "server.log"), {
        "TICKET_ROUTING": "shared",
                                "DATABASE_MAX_CONNECTIONS": "80",
        "HTTP_MAX_IN_FLIGHT_REQUESTS": "200",
    })
    try:
        exports = parse_exports(server.log_path)
        jwt = parse_jwt(server.log_path)
        results = {"events": args.events, "port": args.port}

        # One company for the long history, one small one for projections.
        client = Client(args.port, server.tokens)
        client.trigger("SignUpCompany", {
            "company_id": "rs-company", "name": "Read Side Co",
            "contact_email": "bench@readside.example"})
        client.trigger("SignUpCompany", {
            "company_id": "rs-small", "name": "Small Co",
            "contact_email": "bench@small.example"})
        wait_event_count(HELPDESK, "CompanySignedUp", 2)

        say(f"seeding {args.events} TicketCreated events")
        seed_tickets("rs-company", args.events, 8, "rs-hist")
        wait_event_count(HELPDESK, "TicketCreated", args.events)
        seed_tickets("rs-small", 100, 4, "rs-small-ticket")
        wait_event_count(HELPDESK, "TicketCreated", args.events + 100)

        say("queryEvents paging")
        results["query_events"] = measure_query_events(
            jwt, "helpdesk", "TicketCreated", args.events)

        say("alerter catch-up from sequence 0")
        alerter_env = {
            "SKILJ_BASE_URL": f"http://localhost:{args.port}",
            "TICKET_ROUTING": "shared",
            "ALERTER_STATE_FILE": os.path.join(args.out, "alerter-state.json"),
            "ALERTER_POLL_INTERVAL_MS": "50",
            "RUST_LOG": "warn",
        }
        for name in ("TICKET_CREATED_TOKEN", "TICKET_RESOLVED_TOKEN",
                     "TICKET_REOPENED_TOKEN", "TICKET_CLOSED_TOKEN",
                     "TICKET_ESCALATED_TOKEN", "TICKETS_MERGED_TOKEN",
                     "ESCALATE_TICKET_TOKEN"):
            alerter_env[name] = exports[name]
        results["alerter_catch_up"] = catch_up(
            "alerter", args.alerter, alerter_env,
            alerter_env["ALERTER_STATE_FILE"], "created_at", args.events)

        say("engagement-watcher backlog + catch-up from sequence 0")
        seed_activity(args.events, 8)
        wait_event_count(ACTIVITY, "DailyActivityRecorded", args.events)
        watcher_env = {
            "SKILJ_BASE_URL": f"http://localhost:{args.port}",
            "TICKET_ROUTING": "shared",
            "QUIET_AFTER_DAYS": "36500",
            "ENGAGEMENT_WATCHER_STATE_FILE": os.path.join(
                args.out, "engagement-watcher-state.json"),
            "ENGAGEMENT_WATCHER_POLL_INTERVAL_MS": "50",
            "RUST_LOG": "warn",
        }
        for name in ("DAILY_ACTIVITY_RECORDED_TOKEN", "RECORD_ENGAGEMENT_DECLINE_TOKEN"):
            watcher_env[name] = exports[name]
        results["engagement_watcher_catch_up"] = catch_up(
            "engagement-watcher", args.watcher, watcher_env,
            watcher_env["ENGAGEMENT_WATCHER_STATE_FILE"],
            "last_customer_activity", args.events)

        say("projection queries under write load")
        results["projection_load"] = projection_load(
            jwt, "rs-small", "rs-small-ticket-0")

        say("websocket fan-out")
        fanout_levels = [int(x) for x in args.fanout.split(",") if x]
        results["fanout"] = []
        # A fresh process per level: closed websocket connections are not
        # necessarily reaped before the next level's burst of N upgrades,
        # and the previous level's subscribers lingering in the server made
        # the 64-client run after 1+8+32 lose a subset of them. Each level
        # is meant to answer "N concurrent subscribers", not "N on top of
        # whatever the last level left open".
        for n in fanout_levels:
            server.stop()
            server = Server(args.server, DB, args.port,
                            os.path.join(args.out, f"server-fanout-{n}.log"), {
                                "TICKET_ROUTING": "shared",
        "DATABASE_MAX_CONNECTIONS": "80",
                                "HTTP_MAX_IN_FLIGHT_REQUESTS": "200",
                            })
            jwt = parse_jwt(server.log_path)
            token = server.tokens["CreateTicket"]
            # Let the previous phase's connections/transactions drain.
            # Without this, a 64-subscription burst immediately after the
            # 45s write load can lose a subset of subscriptions while the
            # database is still settling (the 1/8/32 levels are
            # unaffected); an isolated 64-subscriber run against a quiet
            # server delivered every event.
            time.sleep(10)
            results["fanout"].append(
                {"clients": n, **fanout(jwt, token, "rs-small", n)})
            say(f"  {n} clients: {results['fanout'][-1]}")

        with open(os.path.join(args.out, "read-side.json"), "w") as f:
            json.dump(results, f, indent=2)
        say(f"wrote {os.path.join(args.out, 'read-side.json')}")
    finally:
        server.stop()
