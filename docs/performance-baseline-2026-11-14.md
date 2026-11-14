# Performance baseline — skilj 0.4.0, 2026-11-14 (#90)

Current-release load, tenant scaling, reads, websocket delivery and allocation
measurements. The previous [0.3 baseline](performance-baseline-2026-11-05.md)
and [buffer investigation](memory-growth-follow-up-2026-11-06.md) are historical
evidence. Neither release's bounded samples establish a lifetime memory bound.

## Reproduction and scope

Run [scripts/bench/current/run.sh](../scripts/bench/current/run.sh) with release
binaries against a dedicated disposable PostgreSQL cluster. The
[driver documentation](../scripts/bench/current/README.md) describes the
workload and optional database CPU/I/O sampler. Exact versions, binary/script
hashes, limits, commands and validation are in
[metadata.json](benchmarks/2026-11-14/metadata.json). Backend source hashes are
in [memory.json](benchmarks/2026-11-14/memory.json).

- Source: `fae51db` plus these uncommitted application fixes; no commit or push
  was made. The measured server binary and every backend source are hashed.
- All skilj crates: 0.4.0; SQLx: 0.9.0; async-nats: 0.51.0. Default Cargo release
  profile and glibc allocator for the primary measurements.
- Machine: Intel Core Ultra 9 185H, 22 logical CPUs, Linux/WSL, about 15.36 GiB
  visible RAM. PostgreSQL 16.15 in Docker Desktop 29.6.1, the same pinned
  `postgres:16-alpine` image digest as the earlier Docker baseline.
- PostgreSQL defaults: 100 connections, 128 MiB shared buffers, 4 MiB work_mem,
  1 GiB max WAL, fsync and synchronous_commit on. Pool/HTTP limits are 80/70
  for load/scaling and 80/200 for read-side. No telemetry exporters or demo
  seed. Background routes, projections and domain loops stay enabled.
- No task-owned build, browser or other benchmark overlapped these runs.
  The host was not reserved: load averages were elevated and are captured
  in samples. The first memory sample was at 1.31 seconds, after traffic had
  begun. Docker CPU/I/O sampling also lengthens sampling intervals. Treat
  the results as observations, not an isolated dependency comparison.

Tenant setup now finishes fixture creation before starting the consumers,
waits for their checkpoints before traffic, and cleans up on setup errors.
This prevents the benchmark from racing its own provisioning/mirroring
against the live consumers. An initial setup-only failure was excluded;
all reported 1/4/16-tenant runs use the corrected harness. Consumer crash,
redelivery and provisioning behavior are tested separately with real binaries.

## Tenant scaling

Each level uses fresh databases, sixteen workers paced at 50 ms, a 60-second
window and co-resident pgbench scale 1 with four clients/two threads. Ticket
walk: create → assign → internal note → resolve. Latencies cover accepted
commands. There were **zero transport failures and zero domain rejections**.

| Tenants | Accepted/s | p50 / p95 / p99 ms | Peak application MiB | pgbench TPS |
|---|---:|---:|---:|---:|
| Shared | 277.22 | 50.66 / 89.30 / 201.46 | 192.1 | 115.9 |
| 1 | 137.14 | 82.65 / 242.11 / 387.02 | 127.9 | 123.2 |
| 4 | 246.31 | 61.49 / 96.00 / 127.64 | 137.3 | 132.9 |
| 16 | 182.92 | 78.42 / 150.01 / 212.07 | 186.0 | 111.2 |

[JSON and raw CSV samples](benchmarks/2026-11-14/) are retained. The one-tenant
point is substantially below the four-tenant point; one short run per level
cannot explain this variability. Repeat on an otherwise idle deployment
machine before tuning pools or claiming a tenant-count regression.

For comparison, the 0.3 Docker baseline recorded 286.65/276.52/246.93/168.27
accepted commands/s for shared/1/4/16 tenants. The current one-tenant outlier,
host activity, Rust toolchain change, harness synchronization and different
measurement dates prevent attributing these differences solely to skilj.
Older native-PostgreSQL reports also changed storage and database settings.

## Reads and websocket delivery

The 1,000-event query returned one full page in **67.70 ms**. That is one page,
not a multi-page history result. Startup, initial catch-up and checkpointing
of the real alerter and engagement watcher reached the 1,000-event target in
0.156s and 0.102s respectively; these are short startup timings, not sustained
consumer throughput estimates.

During thirty seconds with six writers, 1,276 commands were accepted with no
write rejections or read errors. TicketSummary p50/p95/p99 was
24.23/44.92/65.87 ms; CompanyActiveTickets was 25.82/46.18/70.70 ms. The earlier
0.3 run measured 20.01/25.32/33.98 and 21.34/27.05/34.10 ms respectively.
Current tails are higher in this observation; the environmental limitations
above still apply. Ten idle samples per projection are too few for a useful
idle-tail comparison. [Full results](benchmarks/2026-11-14/read-side.json).

| Subscribers | Delivered / expected | Worst event p95 ms | Maximum delivery ms |
|---|---:|---:|---:|
| 1 | 5 / 5 | 99 | 99 |
| 8 | 40 / 40 | 112 | 112 |
| 32 | 160 / 160 | 107 | 109 |
| 64 | 320 / 320 | 111 | 114 |

All subscribers were ready before traffic. Every level used a fresh server
process and delivered every event, with no subscription errors or missing
messages. Timings include command submission/commit and delivery, not just
websocket transport. The 0.3 maxima were 63/59/90/108 ms.

The separate [dashboard read report](dashboard-reads-2026-11-14.md) compares the
old serial customer reads with bounded queue/conversation pages (#102).
That fixture measures the actual application UI path and authorization,
including erased content; the read-side benchmark above measures skilj's
projection and event APIs.

## Sustained load and idle memory

Eighty synchronous workers, one response awaited before the next command,
are paced at 50 ms each. The nominal ceiling is 1,600 requests/s. Actual
results over **184.62 seconds** were 130,384 responses (706.2/s), including
86,215 accepted commands (467.0/s), 35,425 HTTP 503 responses and 8,744 domain
rejections. No transport failure was recorded. The driver continues a ticket
walk after a 503, so later steps may refer to a ticket whose preceding step
failed. Rejection reasons were not sampled. This is overload behavior,
not a claim that every customer journey succeeds.

All-response p50/p95/p99 was **119.17/188.23/274.12 ms**, maximum 652.49 ms.
The earlier 0.3 primary run accepted 78,814 commands over 181.26s (434.8/s),
with p50/p95/p99 130.90/180.44/231.54 ms. These single runs show different
throughput and tail behavior, not a controlled improvement/regression.

Application RSS was **87.3 MiB at the first sample**, 378.0 MiB at the last
load sample, 384.0 MiB at the first idle sample and **461.6 MiB** at the end
of a roughly one-minute foreground-idle phase. CompanyActiveTickets caught
up during idle, while RSS continued growing. The 0.3 primary sample ended
at 481.2 MiB, and its longer quiet-machine follow-up reached 631.7 MiB.
A lower current endpoint does **not** establish that memory growth is fixed.

The dedicated database container used approximately 275.5–441.1% CPU during
load, about 2.8–4.4 CPU cores under Docker's accounting. Its rounded block
I/O counters ended at 237 MB read / 5.2 GB written, including initialization.
These are container totals, separate from application RSS. Per-sample
container memory, connection state, transaction/buffer counters, host CPU
and memory availability are in [memory.json](benchmarks/2026-11-14/memory.json)
and [streamed samples](benchmarks/2026-11-14/memory-samples.jsonl).

## Allocation diagnostic and remaining follow-up

A separate run used the same release binary with jemalloc heap sampling,
90 configured load seconds and 120 idle seconds. It accepted 39,511 commands
over 93.46 actual load seconds. The changed allocator, shorter workload and
profiling overhead make its throughput/RSS unsuitable as substitutes for
primary glibc measurements. [Samples](benchmarks/2026-11-14/memory-profile.json)
and symbolized [load](benchmarks/2026-11-14/allocations-load.json) /
[idle](benchmarks/2026-11-14/allocations-idle.json) snapshots are retained.

The live-allocation estimate was 128.4 MiB near load end and 129.9 MiB near
idle end. The four largest idle stacks total **77.5 MiB** and allocate SQLx
PostgreSQL Bind or receive buffers through `write_chunk_states`,
`write_planned_commands` and `lock_chunk_states`. These are sampled estimates,
not a byte-exact inventory or proof of unreachable objects. The diagnostic's
RSS fluctuated during idle and ended at 243.7 MiB, below its 258.7 MiB idle-start
sample; RSS and live allocations must be interpreted separately.

A timestamped [background cursor snapshot](benchmarks/2026-11-14/background-cursor-snapshot.json)
found CompanyActiveTickets at head 39,590, but
HelpdeskResolutionToArticleSuggestion at 25,836. Knowledge-base writes were
still active, and its global review projection was 546,087 bytes. Foreground
idle therefore included ongoing work that reads/writes growing projection
states and can expand connection buffers.

This reproduces significant SQLx buffer allocations on 0.4, consistent with
the independent [SQLx-only experiment](memory-growth-follow-up-2026-11-06.md).
Upstream [#169](https://github.com/gklijs/SkilJ/issues/169) is closed with buffer
retention guidance and an opt-in pool-hook example in the released library;
it is not evidence that default application RSS is bounded. The concrete
[application follow-up](memory-growth-follow-up-2026-11-14.md) remains open.
No unmeasured buffer-shrinking default was introduced by this work.

## Application validation

All 368 backend tests were verified across the full suite and targeted reruns.
The full run exposed one parallel key-file-creation race in failure_recovery's
fixture; after isolating key paths by database, all three tests in that target
passed, including SIGKILL and PostgreSQL restart. Real NATS tenant email and
revocation tests passed. Consumer fault injection, including activation-feed
recovery and duplicate ticket IDs across contexts, also passed. Clippy passes
with warnings denied, the WASM build passes, and three Chromium/Dex browser
journeys cover authentication, tenant actions, priority-only updates,
reconnection, refresh races and erasure. The final erasure assertion also
compares the UI against actual retained ciphertext values.
