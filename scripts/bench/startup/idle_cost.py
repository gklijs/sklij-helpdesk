#!/usr/bin/env python3
"""Postgres CPU an idle server costs on a given history (issue #30).

    python3 idle_cost.py <server> <template-db> <seconds> <log> [--firing-index]

`--firing-index` first adds the partial index the report proposes
(`deadlines (fire_at) WHERE status = 'firing'`) to the copy.

Copies the template, starts the server with no traffic, waits 20 s, then
measures for <seconds> the CPU time of every Postgres process in the
cluster (utime + stime from /proc, so run nothing else against it) and of
the server itself. Startup found skilj's due-deadline poll doing a full
scan of `deadlines` every tick; this measures what that costs at rest.
"""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "deadline-drain"))
from common import Server, createdb, dropdb, psql  # noqa: E402

FIRING_INDEX = "--firing-index" in sys.argv
BIN, TEMPLATE, SECS, LOG = [a for a in sys.argv[1:] if not a.startswith("--")]
SECS = float(SECS)
DB = f"idle_{TEMPLATE}"
# This cluster's postmaster, not any other Postgres on the box (test
# suites start their own).
PGDATA = os.environ.get("PGDATA", os.path.expanduser("~/.cache/startup-bench/pgdata"))
MASTER = int(open(os.path.join(PGDATA, "postmaster.pid")).readline())
TICK = os.sysconf("SC_CLK_TCK")


# With the cluster started with shared_preload_libraries=pg_stat_statements,
# also print the statements that used the most time during the window.
STATEMENTS = psql("SELECT count(*) FROM pg_extension WHERE extname = 'pg_stat_statements'",
                  "postgres").strip() == "1"
TOP_SQL = """
SELECT round(s.total_exec_time)::text || ' ms | ' || s.calls || ' calls | ' ||
       left(regexp_replace(s.query, '\\s+', ' ', 'g'), 110)
FROM pg_stat_statements s JOIN pg_database d ON d.oid = s.dbid
WHERE d.datname = '{db}' ORDER BY s.total_exec_time DESC LIMIT 8
"""


def stat(pid):
    fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    # fields[1] is ppid; [11..14] utime, stime, cutime, cstime
    return int(fields[1]), [int(x) for x in fields[11:15]]


def cluster_cpu_s():
    """All CPU the cluster has used so far: the postmaster's own plus every
    child it has reaped (cutime/cstime - parallel workers live for one
    query), plus the children alive now."""
    out = subprocess.run(["pgrep", "-P", str(MASTER)], capture_output=True, text=True).stdout
    total = sum(stat(MASTER)[1])
    for pid in (int(p) for p in out.split()):
        try:
            v = stat(pid)[1]
            total += v[0] + v[1]
        except OSError:
            pass
    return total / TICK


def server_cpu_s(pid):
    _, v = stat(pid)
    return (v[0] + v[1]) / TICK


def main():
    dropdb(DB)
    createdb(DB, TEMPLATE)
    n = psql("SELECT count(*) FROM bc_helpdesk.deadlines", DB).strip()
    if FIRING_INDEX:
        psql("CREATE INDEX deadlines_firing ON bc_helpdesk.deadlines (fire_at) "
             "WHERE status = 'firing'", DB)
    server = Server(BIN, DB, 18451, LOG)
    try:
        time.sleep(20)
        if STATEMENTS:
            psql("SELECT pg_stat_statements_reset()", "postgres")
        pg0, sv0, t0 = cluster_cpu_s(), server_cpu_s(server.proc.pid), time.time()
        time.sleep(SECS)
        pg1, sv1, t1 = cluster_cpu_s(), server_cpu_s(server.proc.pid), time.time()
        top = psql(TOP_SQL.format(db=DB), "postgres") if STATEMENTS else ""
    finally:
        server.stop()
        dropdb(DB)
    secs = t1 - t0
    print(f"{TEMPLATE}{' +firing-index' if FIRING_INDEX else ''}: {n} deadline rows; idle {secs:.0f}s: Postgres CPU {pg1 - pg0:.1f}s "
          f"({100 * (pg1 - pg0) / secs:.0f}% of a core), server CPU {sv1 - sv0:.1f}s", flush=True)
    for line in top.splitlines():
        print("   ", line, flush=True)


if __name__ == "__main__":
    main()
