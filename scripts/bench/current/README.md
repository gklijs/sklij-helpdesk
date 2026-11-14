# Current-release performance baseline

Run against a dedicated, disposable PostgreSQL 15+ cluster. The memory
workload creates a unique database and drops it afterwards. The existing
read-side and scaling scripts recreate their named databases. Do not point
these scripts at a production cluster.

```sh
cargo build --locked --release --bins
export PGBIN="$HOME/.theseus/postgresql/18.6.0/bin" # psql/createdb/pgbench
export PGHOST=127.0.0.1 PGPORT=55432 PGUSER=postgres PGPASSWORD=postgres
bash scripts/bench/current/run.sh /tmp/helpdesk-030-baseline
```

Requires Python 3, Node, `ws` (the fan-out helper), and the PostgreSQL client
utilities. Keep the box quiet and run no other benchmark concurrently.
The driver uses the configured release binaries; it does not build them.

The memory workload runs 80 synchronous workers, at most one command per
50 ms each (nominal ceiling 1,600/s), for 180 seconds. Each ticket goes
through create, assign, resolve, close, rate; rating removes it from the
active queue. It then samples 60 seconds with no foreground traffic.
`memory.json` records actual response counts, errors/rejections, latency
percentiles, 30-second windows, per-process RSS/HWM, database active/idle
connections, source hashes, commit, Rust version and machine information. Set `BENCH_DATABASE_CONTAINER` to the disposable
Docker container name to additionally sample database CPU, memory and block I/O
with `docker stats`, plus database transaction/buffer counters.
Nominal rate is not achieved rate: workers wait for each response.

Scaling uses shared/1/4/16 tenants, sixteen workers at 50 ms, 60 seconds per
level, and a co-resident pgbench scale-1 database. Pools and HTTP limits
are 80/70. Read-side uses 1,000 tickets, thirty seconds of concurrent
writers and fan-out to 1/8/32/64 sockets, five events per level. Read-side retains
its historical pool/HTTP limits of 80/200; the six concurrent writers do
not approach that HTTP limit.
Telemetry exporters are disabled; background domain and projection loops
remain enabled. Preserve JSON/CSV outputs with the report; server logs
contain temporary credentials and must not be published.

These are bounded, small-history baselines. Compare historical numbers
only with the differences in hardware, history size, duration and limits
stated. A short plateau does not establish a lifetime memory bound. If
RSS continues growing abnormally, capture allocations on the same
workload (for example with heaptrack) before claiming a leak or a fix.

For a diagnostic allocation capture with a profiling-enabled jemalloc:

```sh
python3 scripts/bench/current/memory.py target/release/server /tmp/helpdesk-030-profile \
  --seconds 180 --idle-seconds 180 \
  --jemalloc-profile /usr/lib/x86_64-linux-gnu/libjemalloc.so.2
```

This changes allocator behavior and adds sampling overhead; its throughput
and RSS are diagnostic, not interchangeable with the default-allocator run.
Profiles are emitted per GiB of allocations and at exit. The server log
contains jemalloc's final allocation/active/resident statistics. Use the
matching jemalloc `jeprof` script with the exact measured server binary to
symbolize the `.heap` files. Inspect profiles during load as well as at exit,
since graceful shutdown releases the application's live state.

## Connection-buffer isolation

The Linux-only `examples/connection_buffers.rs` reproduction uses SQLx
alone, with no skilj runtime or caches. It holds one or 32 sockets, drains
four queries with a 2 MiB parameter, measures idle RSS, shrinks buffers and
then completes a small query on the same sockets before closing them.

```sh
cargo build --locked --release --example connection_buffers
DATABASE_URL=postgres://... target/release/examples/connection_buffers 32 2097152
```

Use this to distinguish documented client buffer retention from growing
event caches. It is a diagnostic, not a production recommendation to
shrink after every query. See the [investigation](../../../docs/memory-growth-follow-up-2026-11-06.md)
and [SQLx documentation](https://docs.rs/sqlx/0.9.0/sqlx/trait.Connection.html#tymethod.shrink_buffers).
The memory driver now streams samples to `memory-samples.jsonl`, including
host CPU busy percentage, available memory and the start timestamp.
