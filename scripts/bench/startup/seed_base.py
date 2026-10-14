#!/usr/bin/env python3
"""Seeds the real base history that clone.py multiplies (issue #30).

    python3 seed_base.py <server-binary> <db> <minutes> <log-dir>

Runs the server's built-in demo traffic (SEED_DEMO_TRAFFIC, 80 workers x
50 ms over 50 companies: tickets, assignments, notes, merges, ratings,
resolutions) for <minutes>, which also fills `knowledge_base` and
`marketing` through their routes. A thread alongside submits
`RecordDailyActivity` over REST for synthetic (person, day) pairs, so
`activity` has history too (in production the frontend submits it on page
load). Then restarts once with no traffic and waits for every loop to
catch up (../deadline-drain/settle.py, then the deadline cursors), so the
base starts settled.
"""
import os
import re
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "deadline-drain"))
from common import Client, Server, createdb, dropdb, psql  # noqa: E402

BIN, DB, MINUTES, LOGS = sys.argv[1], sys.argv[2], float(sys.argv[3]), sys.argv[4]
PORT = 18301
os.makedirs(LOGS, exist_ok=True)
DEMO = {"SEED_DEMO_TRAFFIC": "1", "SEED_DEMO_CONCURRENCY": "80",
        "SEED_DEMO_INTERVAL_MS": "50", "SEED_DEMO_COMPANIES": "50"}


def activity_token(log_path):
    for line in open(log_path):
        m = re.match(r"^  RecordDailyActivity: (\S+)$", line.rstrip("\n"))
        if m:
            return m.group(1)
    raise RuntimeError("no RecordDailyActivity token in " + log_path)


def companies():
    return psql("SELECT DISTINCT tag_value FROM bc_helpdesk.event_tags WHERE tag_key = 'company'",
                DB).split() or ["acme"]


def activity_worker(server, token, stop, counts):
    client = Client(server.port, {"RecordDailyActivity": token})
    cos = companies()
    n = 0
    while not stop.is_set():
        if n % 500 == 0:
            cos = companies()
        co = cos[n % len(cos)]
        kind = "staff" if n % 4 == 0 else "customer"
        day = f"2026-{1 + (n // 3000) % 12:02d}-{1 + (n // 100) % 28:02d}"
        status, accepted, _ = client.trigger("RecordDailyActivity", {
            "company_id": co, "person_kind": kind,
            "person_subject": f"{kind}-{n % 97}", "day": day})
        counts["ok" if accepted else "rejected"] += 1
        n += 1
        time.sleep(0.01)


def settle_deadlines():
    """settle.py watches sequences and projection cursors, not deadline
    cursors - and skilj's schedule/cancel loops fall far behind a burst
    (they catch up at ~130 source events/s), so idle on until every
    deadline cursor has stopped moving for 30s."""
    server = Server(BIN, DB, PORT, f"{LOGS}/seed-deadlines.log")
    q = ("SELECT string_agg(last_dispatched_sequence::text, ',' ORDER BY cursor_owner) "
         "FROM bc_helpdesk.deadline_cursors")
    prev, still, t0 = None, 0, time.time()
    try:
        while still < 6:
            row = psql(q, DB)
            still = still + 1 if row == prev else 0
            prev = row
            time.sleep(5)
    finally:
        server.stop()
    print(f"  deadline cursors settled after {time.time() - t0:.0f}s: {prev}", flush=True)


def main():
    dropdb(DB)
    createdb(DB)
    server = Server(BIN, DB, PORT, f"{LOGS}/seed-traffic.log", DEMO)
    stop, counts = threading.Event(), {"ok": 0, "rejected": 0}
    t = threading.Thread(target=activity_worker,
                         args=(server, activity_token(f"{LOGS}/seed-traffic.log"), stop, counts))
    t.start()
    try:
        end = time.time() + MINUTES * 60
        while time.time() < end:
            time.sleep(30)
            print(f"  {time.strftime('%T')} helpdesk head "
                  f"{psql('SELECT next_value FROM bc_helpdesk.sequence', DB)}, activity {counts}",
                  flush=True)
    finally:
        stop.set()
        t.join()
        server.stop()
    subprocess.run([sys.executable, os.path.join(os.path.dirname(__file__), "..", "deadline-drain",
                                                 "settle.py"), BIN, DB, f"{LOGS}/seed-settle.log"],
                   check=True)
    settle_deadlines()
    for schema in ("helpdesk", "activity", "marketing", "knowledge_base"):
        print(schema, psql(f"SELECT count(*) FROM bc_{schema}.events", DB))


if __name__ == "__main__":
    main()
