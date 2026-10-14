# Startup benchmark (issue #30)

How long the server takes to start on a large history, what a restart costs
under load, and what an idle server costs Postgres. Results:
`docs/startup-report-2026-10-14.md`.

Standard library Python only; reuses `../deadline-drain/common.py`, so the
same `PGHOST`/`PGPORT`/`PGUSER`/`PGBIN` apply (default `127.0.0.1:5440`).
`idle_cost.py` also reads the cluster's `PGDATA` (default
`~/.cache/startup-bench/pgdata`) to find its postmaster.

## 1. History (~30 minutes)

    python3 seed_base.py <server> base 6 <log-dir>     # ~245k events, then settles
    python3 clone.py base h1m 4                          # 1.2M events, ~2.5 min
    python3 clone.py base h3m 12                         # 3.2M events, ~7.5 min

`seed_base.py` runs the built-in demo traffic plus `RecordDailyActivity`,
then idles until projections, routes and deadline cursors have caught up
(the deadline cursors alone take ~12 minutes after a 6-minute burst).
`clone.py` copies every context's history in SQL with shifted sequences and
suffixed ids, and moves every cursor, so the result starts settled.

The seed's SLA deadlines fall due within hours. For runs later than that,
move them out first, or the server fires them during the run:

    UPDATE bc_helpdesk.deadlines SET fire_at = fire_at + interval '7 days'
    WHERE status = 'pending';

## 2. Runs

    ./run-all.sh <server> <out-dir>                      # cold, restart-load, peer-load
    python3 startup.py <server> h3m <label> <out> --mode cold|restart-load|peer-load
    python3 idle_cost.py <server> h3m 60 <log> [--firing-index]

`idle_cost.py` prints the top statements when the cluster was started with
`shared_preload_libraries=pg_stat_statements` and the extension exists in
the `postgres` database. Never run two at once; keep the box quiet.
