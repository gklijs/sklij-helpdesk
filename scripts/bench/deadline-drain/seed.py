#!/usr/bin/env python3
"""Seeds the two template databases the drain runs copy (issue #28).

usage: seed.py <server-binary> <tickets> <companies> <threads> <workdir>

Every row is real: tickets go through REST like any client's, and skilj's
own schedule/cancel loops write the deadline rows. Produces:

- `dd_created`  - <tickets> tickets created and assigned (in progress),
                  each with its two pending SLA deadlines
                  (ScheduleFirstResponseSla / ScheduleResolutionSla).
- `dd_resolved` - the same tickets resolved: each has one pending
                  ScheduleTicketAutoClose deadline (7 days out) and two
                  cancelled SLA deadlines.

Both are left for `drain.py` to copy with CREATE DATABASE ... TEMPLATE.
"""
import sys
import threading
import time

from common import (Client, Server, createdb, deadline_counts, dropdb, psql,
                    ticket_payload)

BIN, N, C, T, WORK = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
DB = "dd_seed"
PORT = 18191


def run_parallel(server, fn, label):
    errors = []
    done = [0]
    lock = threading.Lock()

    def worker(k):
        client = Client(server.port, server.tokens)
        for i in range(k, N, T):
            try:
                fn(client, i)
            except Exception as e:  # noqa: BLE001 - recorded, reported below
                with lock:
                    errors.append(f"{i}: {e}")
            with lock:
                done[0] += 1

    t0 = time.time()
    threads = [threading.Thread(target=worker, args=(k,)) for k in range(T)]
    for t in threads:
        t.start()
    while any(t.is_alive() for t in threads):
        time.sleep(10)
        el = time.time() - t0
        print(f"  {label}: {done[0]}/{N} tickets, {done[0] / el:.0f} tickets/s", flush=True)
    for t in threads:
        t.join()
    print(f"  {label}: done in {time.time() - t0:.0f}s, {len(errors)} errors", flush=True)
    for e in errors[:20]:
        print("   ", e)
    if errors:
        sys.exit(f"{label}: errors, see above")


def must(client, command, payload):
    status, accepted, _ = client.trigger(command, payload)
    if status != 200 or not accepted:
        raise RuntimeError(f"{command} {payload['ticket_id'] if 'ticket_id' in payload else payload}: "
                           f"http {status} accepted={accepted}")


def create_and_assign(client, i):
    tid = f"dd-{i}"
    must(client, "CreateTicket", ticket_payload(tid, f"ddco-{i % C}", f"cust-{i % 997}"))
    must(client, "AssignTicket", {"ticket_id": tid, "staff_id": f"staff-{i % 40}"})


def resolve(client, i):
    must(client, "ResolveTicket", {"ticket_id": f"dd-{i}"})


def wait_for(db, pred, what):
    t0 = time.time()
    while True:
        counts = deadline_counts(db)
        if pred(counts):
            print(f"  {what}: settled after {time.time() - t0:.0f}s: {counts}", flush=True)
            return
        if time.time() - t0 > 600:
            sys.exit(f"{what}: never settled: {counts}")
        time.sleep(2)


def main():
    for db in (DB, "dd_created", "dd_resolved"):
        dropdb(db)
    createdb(DB)

    print("phase 1: sign up companies, create + assign tickets", flush=True)
    server = Server(BIN, DB, PORT, f"{WORK}/seed-1.log")
    client = Client(server.port, server.tokens)
    for c in range(C):
        must(client, "SignUpCompany", {"company_id": f"ddco-{c}", "name": f"Co {c}",
                                       "contact_email": f"ops{c}@x.example"})
    try:
        run_parallel(server, create_and_assign, "create+assign")
        wait_for(DB, lambda k: k.get(("ScheduleFirstResponseSla", "pending"), 0) == N
                 and k.get(("ScheduleResolutionSla", "pending"), 0) == N, "SLA schedules")
        time.sleep(5)  # let async projections finish too
    finally:
        server.stop()
    createdb("dd_created", DB)

    print("phase 2: resolve every ticket", flush=True)
    server = Server(BIN, DB, PORT, f"{WORK}/seed-2.log")
    try:
        run_parallel(server, resolve, "resolve")
        wait_for(DB, lambda k: k.get(("ScheduleTicketAutoClose", "pending"), 0) == N
                 and k.get(("ScheduleFirstResponseSla", "cancelled"), 0) == N
                 and k.get(("ScheduleResolutionSla", "cancelled"), 0) == N, "auto-close schedules")
        time.sleep(5)
    finally:
        server.stop()
    createdb("dd_resolved", DB)
    dropdb(DB)
    print(psql("SELECT next_value FROM bc_helpdesk.sequence", "dd_resolved"))


if __name__ == "__main__":
    main()
