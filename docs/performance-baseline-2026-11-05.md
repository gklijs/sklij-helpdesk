# Performance baseline — skilj 0.3.0, 2026-11-05

Current-release evidence for issue #90, superseding its original 0.2.0 target. This is a bounded baseline on fresh databases, with a separate allocation diagnostic. It does not establish long-term memory stability or a regression against a different database environment.

## Reproduction

Run [`scripts/bench/current/run.sh`](../scripts/bench/current/run.sh) after building the release binaries. The [driver README](../scripts/bench/current/README.md) contains prerequisites, exact workloads and the allocation-capture command. [Metadata](benchmarks/2026-11-05/metadata.json), [memory samples and windows](benchmarks/2026-11-05/memory.json), scaling JSON/CSV and [read results](benchmarks/2026-11-05/read-side.json) are retained beside this report. Temporary server logs contain command credentials and are excluded.

- Source: `7ccb64793d2e3bf46c2d671b6c5b54fb33999b84` plus the working-tree upgrade changes. Per-source SHA-256 values are in `memory.json`; the measured server binary is `493aab0c63c2a7ed155a07f00e6e6c9d2b670ac2d366f98c42b633b023d17f8d`.
- Dependency: all skilj crates 0.3.0; async-nats 0.51.0. Rust 1.99.1; default release profile, no custom allocator in the primary run.
- Machine: Intel Core Ultra 9 185H, 22 logical CPUs, 15.36 GiB visible RAM, Linux/WSL. Database: PostgreSQL 16.15 in Docker Desktop 29.6.1, `postgres:16-alpine`; exact image digest and Docker resources are in metadata. Storage is Docker-managed, not the native cluster used by earlier reports.
- Database: max_connections=100, shared_buffers=128 MiB, work_mem=4 MiB, max_wal_size=1 GiB, fsync=on, synchronous_commit=on. Memory/scaling server pool=80, HTTP in-flight limit=70. Read-side retains 80/200. Background domain/projection loops remain enabled. Console tracing is enabled at warn level; OpenTelemetry SDK providers and exporters are disabled; no demo seed, Dex or UI traffic.
- Runs execute serially. Initial databases are empty. Scaling is 60 seconds per level with sixteen workers paced at 50 ms and pgbench scale 1, four clients/two threads on a sibling database. Read-side is 1,000 initial tickets, thirty seconds of concurrent writes and five events at each fan-out level.

## Tenant scaling

Latency percentiles below cover accepted commands. The walk is create → assign → internal note → resolve. There were no transport failures or domain rejections at any level. RSS is sampled once per second.

| Tenants | Accepted/s | p50 / p95 / p99 ms | Peak server MiB | Co-resident pgbench TPS |
|---|---:|---:|---:|---:|
| Shared | 286.65 | 51.43 / 79.30 / 94.75 | 196.8 | 151.7 |
| 1 | 276.52 | 50.85 / 74.33 / 259.50 | 148.7 | 246.3 |
| 4 | 246.93 | 60.28 / 96.95 / 141.25 | 150.1 | 133.8 |
| 16 | 168.27 | 87.05 / 166.08 / 218.71 | 189.1 | 154.5 |

The 16-tenant result deserves a repeat on the deployment database before changing pool sizes. The old [0.0.10 scaling report](tenant-scaling-report-2026-10-15.md) measured about 317/280/320/320 commands/s for shared/1/4/16 tenants. Its native PostgreSQL 18.6 had 512 MiB shared buffers, 400 connections, an 8 GiB WAL limit, 120/200 application limits, pgbench scale 10 and 240-second windows. Its pgbench throughput was also much higher (roughly 559–673 TPS). These runs do not isolate dependency performance; a percentage regression claim would conflate storage, database configuration, duration and load.

## Reads and websocket delivery

The 1,000-event query returned its full page in 56.03 ms. This sample exercises one full page, not multi-page history. Catch-up of the real alerter and engagement watcher took 0.152s and 0.052s; these short timings include startup and checkpoint writes and are not steady-state throughput estimates.

Under six writer threads, 1,521 commands were accepted in thirty seconds; there were no read errors or write rejections. TicketSummary read p50/p95/p99 was 20.01/25.32/33.98 ms; CompanyActiveTickets was 21.34/27.05/34.10 ms. Ten idle samples per projection are recorded in the JSON; that sample count is too small for a useful idle-tail comparison.

| Subscribers | Delivered / expected | Worst event p95 ms | Maximum delivery ms |
|---|---:|---:|---:|
| 1 | 5 / 5 | 63 | 63 |
| 8 | 40 / 40 | 59 | 59 |
| 32 | 160 / 160 | 86 | 90 |
| 64 | 320 / 320 | 108 | 108 |

All subscribers were ready before traffic, and every level used a fresh server process. There were no subscription errors or missing deliveries. Times include command submission/commit and websocket delivery, not websocket transport alone. The earlier [read-side methodology](../scripts/bench/read-side/README.md) uses a 5,000-event history and 45-second write phase, so these latency samples are current observations rather than a controlled historical comparison.

## Sustained load and idle memory

Eighty workers each wait for a response before the next command, paced at 50 ms: the nominal ceiling is 1,600 requests/s. The actual run produced 122,889 responses over 181.26s (678.0/s), including 78,814 accepted commands (434.8/s), 34,618 HTTP 503 responses and 9,457 domain rejections. No transport error was recorded. The driver continues a ticket walk after a 503, so later steps can refer to an uncreated or untransitioned ticket; individual rejection reasons were not sampled. This is overload evidence, not an all-successful customer journey.

All-response p50/p95/p99 was 130.90/180.44/231.54 ms, maximum 366.87 ms. Accepted commands per 30-second window were 14,545, 12,430, 13,384, 13,160, 12,643 and 12,149. Database samples generally had active background work even after foreground traffic stopped; connection counts and host load averages are retained in the JSON. Database CPU and I/O utilization were not independently profiled.

RSS grew from 45.7 MiB at the first sample to 418.3 MiB at the last load sample. After sixty seconds without foreground traffic it reached 481.2 MiB. A one-minute idle phase therefore does **not** demonstrate a bound or distinguish catch-up from retained allocations. The historical [0.0.9 report](load-test-report-2026-10-01.md) reached 4.57 GB at nominal 1,600/s, but used the demo seed with different command flows, four-minute windows and a native database. The smaller value here is not evidence that the historical problem was fixed.

## Allocation diagnostic

The diagnostic repeated the same eighty-worker, three-minute workload with jemalloc 5.3.0 sampling, followed by **three minutes** without foreground traffic. This changes the allocator and adds profiling overhead; its throughput and RSS cannot be substituted for the primary glibc results. [Samples and projection lag](benchmarks/2026-11-05/memory-profile.json), [allocator metadata](benchmarks/2026-11-05/allocation-metadata.json) and symbolized [load](benchmarks/2026-11-05/allocations-load.txt), [idle](benchmarks/2026-11-05/allocations-idle.txt) and [exit](benchmarks/2026-11-05/allocations-exit.txt) snapshots are retained.

The diagnostic accepted 86,679 commands. RSS was 312.6 MiB at the start of idle and 401.2 MiB at the last sample, with a 412.0 MiB high-water mark. CompanyActiveTickets lag reached zero within the first thirty idle seconds and remained zero, while RSS continued rising. **Ticket projection backlog alone does not explain the growth.** Other background loops remained active.

Sampled live allocation estimates were 176.2 MiB during load and 272.1 MiB near the end of idle. Load stacks include SQL encoding/growing buffers under `write_planned_commands` and `commit_command_batch`; idle stacks include `fold_catch_up_chunk`, `lock_chunk_states` and `write_chunk_states`. The original jeprof output stripped Rust generic names and warned on some symbols. A subsequent direct ELF-symbol resolver preserved the names and matched the 272.1 MiB total; its five largest stacks account for 236.5 MiB in SQLx Bind/receive buffers. The [next-day investigation](memory-growth-follow-up-2026-11-06.md) includes these stacks and a SQLx-only reproduction.

After graceful shutdown, jemalloc reported 1.43 MiB allocated, 6.90 MiB active and 187.11 MiB resident, with 846.20 MiB retained virtual address space. The exit sample estimated 0.5 MiB live. This supports investigating long-lived buffers and allocator retention before claiming unreachable leaked objects, but it does **not** establish a steady-state bound while the server runs. The [focused follow-up](memory-growth-follow-up-2026-11-06.md) now identifies SQLx buffer retention as a major cause and links the upstream documentation/policy issue.

## Validation

Formatting, backend Clippy with warnings denied, WASM build, PostgreSQL-backed domain/integration tests, tenant email restart/redelivery tests and the real Chromium smoke journey accompany the upgrade. The browser regression uses two staff sessions and a customer session; the observing staff feed receives only TicketPriorityChanged so another macro event cannot hide the missing notification. The host-specific embedded PostgreSQL restart scenario is verified separately with its compatible libxml2 path.
