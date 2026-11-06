# Memory investigation: retained SQLx connection buffers

**Finding:** a major part of the measured application memory is retained SQLx connection buffers. A separate reproduction needs neither skilj nor its event cache. This matches SQLx's documented behavior; no unreachable-memory leak in skilj is demonstrated. [Skilj issue #169](https://github.com/gklijs/SkilJ/issues/169) requests documentation and evaluation of bounded reclamation after large operations.

## Controlled repeat on the idle machine

The original [0.3.0 baseline](performance-baseline-2026-11-05.md) was repeated with the **same server binary hash**, pinned PostgreSQL image, default glibc allocator, pool=80, HTTP limit=70 and eighty workers. No builds, tests or browser runs overlapped. Initial load averages were about 0.10/0.08/0.30; host CPU counters and memory availability are now sampled too.

Three minutes of load accepted 91,648 commands; the driver counted 34,648 HTTP 503 responses and 9,362 downstream domain rejections. Five minutes without foreground requests followed. Application RSS was 54.1 MiB initially, 446.4 MiB at idle start, and 631.7 MiB at the end. It stayed exactly at 631.7 MiB for the final approximately 55 seconds. This run shows a short plateau, not a lifetime bound, but concurrent tests are not required to reproduce the rise.

See [samples](benchmarks/2026-11-06/idle-repeat-memory.json), [streamed measurements](benchmarks/2026-11-06/idle-repeat-memory-samples.jsonl) and [metadata](benchmarks/2026-11-06/metadata.json). The process RSS excludes the separate PostgreSQL server's allocations.

## Foreground idle was still background work

CompanyActiveTickets caught up within the first thirty idle seconds. However, a [cursor snapshot](benchmarks/2026-11-06/background-cursor-snapshot.json) showed the resolution-to-article route only at sequence 53,238 against a helpdesk head of 91,727, while knowledge-base events and projections were still being written.

`KnowledgeBaseReview` is one global JSON state containing every pending article suggestion. Each successfully resolved ticket can add a suggestion. As that queue grows, projection folding reads and rewrites a larger state. SQLx connections serving those operations can retain larger message buffers, including as background processing spreads across the pool. This is real ongoing work after the HTTP driver stops.

The event cache is separately capped at 1,000 events per context in this configuration. It contributes memory but does not explain the dominant allocation stacks or the independent reproduction below.

## Heap attribution without stripping Rust generic names

The original jemalloc capture used an optimized release binary. Old jeprof formatting reduced important generic names to misleading short names such as `::clone`. [heap_stacks.py](../scripts/bench/current/heap_stacks.py) instead resolves recorded return addresses directly against the exact ELF symbol table and applies the same heap_v2 sampling correction. It reproduces jeprof's total: 272.091 MiB versus 272.1 MiB.

The [five largest stacks](benchmarks/2026-11-06/raw-heap-stacks.json), totaling about 236.5 MiB, are:

| Estimated MiB | Allocation path |
|---:|---|
| 98.24 | PostgreSQL Bind encode buffer → projection `write_chunk_states` |
| 64.41 | BytesMut receive buffer → projection `lock_chunk_states` |
| 48.08 | PostgreSQL Bind encode buffer → `write_planned_commands` |
| 21.70 | PostgreSQL Bind encode buffer → projection `write_chunk_states` |
| 4.08 | PostgreSQL Bind encode buffer → `write_planned_commands` |

These are sampled estimates, not a byte-exact allocation inventory. They identify client buffer allocation paths; source inspection and the separate experiment establish the retention behavior.

## SQLx-only experiment

[connection_buffers.rs](../examples/connection_buffers.rs) opens and holds the same connections, sends a 2 MiB parameter through four fully drained SELECTs per connection, drops every result, waits fifteen seconds, calls `shrink_buffers()`, completes a small query on each socket, then closes the pool. There is no skilj runtime, event cache, projection, or application history.

```sh
cargo build --locked --release --example connection_buffers
DATABASE_URL=postgres://... target/release/examples/connection_buffers 32 2097152
```

The example samples Linux process RSS. Run it against a disposable database, outside other benchmarks.

| Stage | One connection MiB | 32 connections MiB |
|---|---:|---:|
| Warm pool | 9.32 | 9.80 |
| After four query passes | 17.38 | 186.04 |
| After fifteen idle seconds | 17.38 | 186.04 |
| Immediately after shrinking | 15.38 | 124.37 |
| After a completed small query | 15.38 | 90.30 |
| Pool closed | 15.37 | 64.35 |

Raw measurements: [one connection](benchmarks/2026-11-06/sqlx-buffers-1.jsonl), [32 connections](benchmarks/2026-11-06/sqlx-buffers-32.jsonl). Repeated query passes fluctuate, and RSS does not return completely to startup values; neither stability under arbitrary workloads nor complete allocator reclamation is claimed. Buffer reclamation clearly reduces memory with the same sockets still open.

## Documentation and policy

[SQLx 0.9 Connection::shrink_buffers](https://docs.rs/sqlx/0.9.0/sqlx/trait.Connection.html#tymethod.shrink_buffers) explicitly documents retained client buffers and their possible appearance as a leak. It also warns that excessive shrinking can cause allocation thrashing and fragmentation. SQLx pool hooks allow caller-supplied maintenance, and skilj already exposes `SkiljBuilder::pool_options`.

Practical guidance:

- Budget for peak client-buffer capacity **per connection**, separately from PostgreSQL backend memory and skilj caches. Larger pools can multiply retained capacity.
- Measure all background cursors before treating a stopped HTTP workload as fully idle.
- Avoid unbounded read-model rows where a keyed or paginated model fits the product. Shrinking buffers does not solve ever-growing application state.
- Do not shrink after every query by default. An occasional or large-operation-triggered policy needs throughput, p95/p99 latency, connection churn and correctness measurements. Idle-only hooks can be ineffective if background polling continually reuses connections.
- Treat RSS separately from live allocations and retained virtual address space. Releasing buffers does not promise that an allocator immediately returns every freed page to the OS.

The upstream issue asks for these docs and a measured opt-in design, rather than an unconditional behavior change. Default production pool behavior remains unchanged. Further work is needed before selecting thresholds or claiming a sustained memory bound across repeated growing workloads.
