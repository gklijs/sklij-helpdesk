# Open follow-up: bound application state and connection-buffer retention

Status: open. This is the focused application follow-up from #90's
[0.4 performance baseline](performance-baseline-2026-11-14.md), with current
allocation evidence. It does not claim a demonstrated unreachable-memory leak.

The default glibc process grew to 461.6 MiB during foreground idle. A separate
jemalloc diagnostic attributed 77.5 MiB of a 129.9 MiB live-allocation estimate
to its four largest SQLx Bind/receive-buffer stacks. The ticket projection was
caught up while the article-suggestion route still had 13,754 source sequences
to examine. The global KnowledgeBaseReview row was already 546,087 bytes and
continued being rewritten. See the baseline's raw snapshots and source/binary
hashes for reproduction.

The upstream documentation request [SkilJ #169](https://github.com/gklijs/SkilJ/issues/169)
is closed. Skilj 0.4 documents retained buffers and offers an opt-in pool-hook
example. The consuming application still needs a measured resource policy.

Work to complete before enabling a production policy:

- Page/key the global review queue so repeated projection work does not copy
  an ever-growing JSON row. Define retention for reviewed suggestions.
- Evaluate occasional or large-operation-triggered `shrink_buffers()` with
  `SkiljBuilder::pool_options`, after operations are fully drained. Compare
  the default, narrow pools and reclamation; avoid shrinking after every query.
- Run at least two growing load/idle cycles with all background cursors tracked.
  Measure RSS, sampled live allocations, accepted throughput, p95/p99 latency,
  database CPU/I/O, connection churn and correctness with large query results.
- Set deployment thresholds from the expected history and memory budget.
  An idle-only hook may not run while background polling reuses connections.

Completion means a repeatable bound under the selected retention/workload
model, with an explicit latency/throughput tradeoff. A short RSS plateau or
freeing allocations on process exit does not meet that criterion.
