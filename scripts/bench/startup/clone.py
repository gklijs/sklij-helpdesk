#!/usr/bin/env python3
"""Multiplies a settled base history in SQL (issue #30).

    python3 clone.py <template-db> <new-db> <copies>

Creates <new-db> from <template-db> and appends <copies> copies of every
bounded context's history, so the result holds (copies + 1) x the
template's events. Real seeding manages ~250 events/s; this does 1M in a
few minutes. Same method as docs/rebuild-report-2026-10-07.md:

- Copy k shifts every sequence by k x H (H = the context's max sequence + 1),
  command ids by k x C, encryption key ids by k x E, and suffixes `-c<k>`
  onto every JSON string under a key ending in `_id`/`subject`, every tag
  value, projection key, snapshot tag value, deadline id and idempotency
  key. Copies never share a key with each other or the original.
- Copied: commands, events (event_tags via skilj's own trigger), encryption
  keys and their event/command links, idempotency keys, projection_state,
  snapshots, deadlines. Not copied: tokens, read cursors' rows,
  parked deliveries, private-field grants, external message keys.
- The id sequences of commands and encryption_keys move past the copies.
- Every cursor moves by the same shift, so a server started on the result
  has no catch-up to do: sequence.next_value, projections and rebuilds'
  caught_up_to, projection/snapshot (partition) progress, deadline and
  cross-context route cursors, read cursors.
- Secondary indexes on the big tables are dropped first and recreated
  after (same definitions, read from pg_indexes), then ANALYZE.

Not exact: ids inside JSON arrays and map keys inside projection state are
not suffixed, so a copy's projection state is close to, not identical
with, what folding its history would give. Startup never folds history it
is caught up on, so this doesn't matter here; don't use the result to
check projection correctness.
"""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "deadline-drain"))
from common import PGBIN, PGHOST, PGPORT, PGUSER, dropdb, psql  # noqa: E402

SRC, DST, K = sys.argv[1], sys.argv[2], int(sys.argv[3])
BIG = ("events", "event_tags", "commands", "projection_state", "snapshots",
       "idempotency_keys", "deadlines", "event_encryption_keys", "command_encryption_keys",
       "encryption_keys")

FUNCS = r"""
CREATE OR REPLACE FUNCTION public.bench_sfx(t text, k int) RETURNS text
  LANGUAGE sql IMMUTABLE AS $$
  SELECT regexp_replace(t, '("[a-z_]*(?:_id|subject)": ?")([^"]*)"', '\1\2-c' || k || '"', 'g') $$;
CREATE OR REPLACE FUNCTION public.bench_sfx_tags(j jsonb, k int) RETURNS jsonb
  LANGUAGE sql IMMUTABLE AS $$
  SELECT regexp_replace(j::text, '("value": ")([^"]*)"', '\1\2-c' || k || '"', 'g')::jsonb $$;
"""


def run_sql(sql, db):
    subprocess.run([f"{PGBIN}/psql", "-h", PGHOST, "-p", PGPORT, "-U", PGUSER, "-d", db,
                    "-v", "ON_ERROR_STOP=1", "-q", "-c", sql], check=True)


def one(sql, db):
    out = psql(sql, db).strip()
    return int(out) if out else 0


def clone_context(s):
    sc = f"bc_{s}"
    h = one(f"SELECT coalesce(max(sequence), -1) + 1 FROM {sc}.events", DST)
    if h == 0:
        print(f"  {s}: no events, skipped", flush=True)
        return 0
    c = one(f"SELECT coalesce(max(id), 0) + 1 FROM {sc}.commands", DST)
    e = one(f"SELECT coalesce(max(id), 0) + 1 FROM {sc}.encryption_keys", DST)
    # Indexes backing a constraint (primary keys, UNIQUE) stay: they can't
    # be dropped on their own, and the inserts need them anyway.
    idx = psql(f"SELECT i.indexname || '|' || i.indexdef FROM pg_indexes i "
               f"WHERE i.schemaname = '{sc}' AND i.tablename IN ({','.join(repr(t) for t in BIG)}) "
               f"AND NOT EXISTS (SELECT 1 FROM pg_constraint c "
               f"WHERE c.conindid = format('%I.%I', i.schemaname, i.indexname)::regclass)",
               DST).splitlines()
    idx = [line.split("|", 1) for line in idx if line]
    gs = f"generate_series(1, {K}) AS k"
    shift = f"{K} * {h}"
    sql = f"""
BEGIN;
{''.join(f'DROP INDEX {sc}.{name};' for name, _ in idx)}
INSERT INTO {sc}.commands SELECT id + k * {c}, external_id || '-c' || k, command_type_name,
  public.bench_sfx(payload, k), metadata_type, metadata_version, metadata_client_id,
  metadata_created_at, metadata_correlation_id, metadata_causation_id, consistency_tags,
  consistency_boundary + k * {h}, consistency_query
  FROM {sc}.commands, {gs} WHERE id < {c};
INSERT INTO {sc}.events SELECT sequence + k * {h}, event_type_name, public.bench_sfx(payload, k),
  metadata_type, metadata_version, metadata_client_id, metadata_created_at,
  metadata_correlation_id, metadata_causation_id, public.bench_sfx_tags(tags, k), origin_kind,
  origin_source_content, origin_source_context, origin_command_id + k * {c}
  FROM {sc}.events, {gs} WHERE sequence < {h};
INSERT INTO {sc}.encryption_keys SELECT id + k * {e}, subject_key, subject_value || '-c' || k,
  status, created_at, destroyed_at, wrapped_key, wrap_nonce
  FROM {sc}.encryption_keys, {gs} WHERE id < {e};
INSERT INTO {sc}.event_encryption_keys SELECT event_sequence + k * {h}, encryption_key_id + k * {e}
  FROM {sc}.event_encryption_keys, {gs} WHERE event_sequence < {h};
INSERT INTO {sc}.command_encryption_keys SELECT command_id + k * {c}, encryption_key_id + k * {e}
  FROM {sc}.command_encryption_keys, {gs} WHERE command_id < {c};
INSERT INTO {sc}.idempotency_keys SELECT command_type_name, client_id, idempotency_key || '-c' || k,
  (SELECT array_agg(x + k * {h}) FROM unnest(triggered_event_sequences) x), created_at
  FROM {sc}.idempotency_keys, {gs};
INSERT INTO {sc}.projection_state SELECT p.projection_name, p.key || '-c' || k,
  public.bench_sfx(p.state, k), p.owner || '-c' || k, p.as_of_sequence + k * {h}, p.updated_at
  FROM {sc}.projection_state p, {gs};
INSERT INTO {sc}.snapshots SELECT snapshot_name, tag_key, tag_value || '-c' || k, snapshot_version,
  as_of_sequence + k * {h}, public.bench_sfx(state::text, k)::jsonb, owner || '-c' || k, updated_at
  FROM {sc}.snapshots, {gs};
INSERT INTO {sc}.deadlines SELECT id || '-c' || k, schedule_name, fire_at,
  public.bench_sfx_tags(tags, k), correlation_id, target_bounded_context, target_command_type,
  public.bench_sfx(payload, k), status, created_at, resolved_at, firing_at, attempt_count,
  first_failed_at, next_attempt_at, last_error
  FROM {sc}.deadlines, {gs};
SELECT setval('{sc}.commands_id_seq', (SELECT max(id) FROM {sc}.commands));
SELECT setval('{sc}.encryption_keys_id_seq', greatest(1, (SELECT max(id) FROM {sc}.encryption_keys)));
UPDATE {sc}.sequence SET next_value = next_value + {shift};
UPDATE {sc}.projections SET caught_up_to = caught_up_to + {shift} WHERE caught_up_to >= 0;
UPDATE {sc}.projection_rebuilds SET caught_up_to = caught_up_to + {shift} WHERE caught_up_to >= 0;
UPDATE {sc}.projection_partition_progress SET caught_up_to = caught_up_to + {shift} WHERE caught_up_to >= 0;
UPDATE {sc}.snapshot_progress SET caught_up_to = caught_up_to + {shift} WHERE caught_up_to >= 0;
UPDATE {sc}.snapshot_partition_progress SET caught_up_to = caught_up_to + {shift} WHERE caught_up_to >= 0;
UPDATE {sc}.deadline_cursors SET last_dispatched_sequence = last_dispatched_sequence + {shift}
  WHERE last_dispatched_sequence >= 0;
UPDATE {sc}.cross_context_route_cursors SET last_dispatched_sequence = last_dispatched_sequence + {shift}
  WHERE last_dispatched_sequence >= 0;
UPDATE {sc}.read_cursors SET sequence = sequence + {shift} WHERE sequence >= 0;
{''.join(f'{ddl};' for _, ddl in idx)}
COMMIT;
ANALYZE {sc}.events; ANALYZE {sc}.event_tags; ANALYZE {sc}.commands;
ANALYZE {sc}.projection_state; ANALYZE {sc}.snapshots; ANALYZE {sc}.deadlines;
"""
    t0 = time.time()
    run_sql(sql, DST)
    n = one(f"SELECT count(*) FROM {sc}.events", DST)
    print(f"  {s}: H={h} -> {n} events in {time.time() - t0:.0f}s", flush=True)
    return n


def main():
    dropdb(DST)
    run_sql(f'CREATE DATABASE "{DST}" TEMPLATE "{SRC}"', "postgres")
    run_sql(FUNCS, DST)
    total = 0
    for s in psql("SELECT name FROM public.bounded_contexts ORDER BY name", DST).split():
        total += clone_context(s)
    run_sql("DROP FUNCTION public.bench_sfx(text, int); DROP FUNCTION public.bench_sfx_tags(jsonb, int);",
            DST)
    run_sql("VACUUM ANALYZE", DST)
    print(f"{DST}: {total} events", flush=True)


if __name__ == "__main__":
    main()
