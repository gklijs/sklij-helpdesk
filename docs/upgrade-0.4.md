# Upgrade from skilj 0.3 to 0.4

Skilj 0.4 requires an explicit migration of an existing 0.3 database.
Migration **0010** changes protected event source content and makes the schema
too new for a 0.3 binary. A rollback therefore restores the pre-upgrade
database and matching key-file backup; restarting an old binary against the
migrated database is not a rollback.

## Rehearse on a disposable database

Prerequisites: Docker, Rust, Python 3, the two compiled server versions, and
network access to fetch images/dependencies. The rehearsal provisions its own
PostgreSQL 16 container with an ephemeral local port. It never uses the caller's
DATABASE_URL. Its output directory must be empty; it keeps private evidence
and removes the container and child server even if a check fails.

Build the review baseline and candidate, without changing the checkout:

```sh
baseline_dir="$(mktemp -d /tmp/helpdesk-03.XXXXXX)"
git archive d8ff524a1514a903044b406b688800b80ffa8324 | tar -x -C "$baseline_dir"
cargo build --locked --manifest-path "$baseline_dir/Cargo.toml" --bin server
cargo build --locked --all-targets
python3 scripts/upgrade/rehearse.py \
  --old-server "$baseline_dir/target/debug/server" \
  --new-server target/debug/server \
  --output /tmp/helpdesk-upgrade-evidence \
  --post-migration-tests
```

CI's `upgrade` job performs this baseline build and runs the same script.
The [verified local result](upgrade-evidence-2026-11-14.json) records the binary
hashes and successful checks, including 15 tests on the migrated restore.
The repository must include both this guide and
[scripts/upgrade/rehearse.py](../scripts/upgrade/rehearse.py).

The rehearsal checks:

1. Create a company and an encrypted ticket through the real 0.3 server.
2. Add a historical protected-event source value, then dump and restore the
   populated database into isolated migration and rollback copies.
3. Verify preflight reports `upgrade_needed`, and normal startup with
   `SCHEMA_CHANGES=refuse` refuses before inserting bootstrap roles.
4. Migrate the restore, preserve the event count, verify protected historical
   source content is `[redacted]`, and verify preflight is `up_to_date`.
5. Start 0.4 with schema changes refused; read old encrypted ticket content,
   assign/resolve the old ticket, and erase its customer in this disposable
   fixture. Confirm old plaintext no longer renders through the raw projection.
6. Collect actual OTLP metrics, traces and logs from the running server.
7. Optionally run tenant provisioning/lifecycle, erasure, native deadlines,
   auto-close cancellation, projection rebuild and NATS email suites against
   the restored, migrated database. Silent test skips fail the check.
8. Prove 0.3 refuses the migrated schema, but can start from the untouched backup
   restore and assign the original ticket. Also migrate/start an empty database.

`result.json` contains the checks and binary hashes. The remaining artifacts
include a database backup, encryption keys, credentials in server logs, and
telemetry payloads. Keep the evidence directory private; do not commit or
publish those files. A fresh output directory is required for another run.

## Deployment procedure

1. Back up the database and its **existing** encryption key file. Verify a
   restore before proceeding. Every server instance must continue to use the
   same provider identity and key file; a newly generated file cannot open
   the old wrapped customer keys.
2. Stop all 0.3 servers and consumers, including email, provisioning, lifecycle,
   alerter and engagement processes. Reserve a maintenance window: protected
   source rewriting takes an event-write lock, so duration depends on history.
3. Point the 0.4 server's administrative commands at the intended database and
   existing key file. For a populated deployment, set
   `PREFLIGHT_RECONCILIATION_SUBJECT` to the existing operations role's external
   subject so preflight can inspect the application's registrations.
4. Run `target/debug/server --preflight`. Inspect the JSON verdict and findings;
   `upgrade_needed` is expected for a 0.3 schema. Do not bypass a `refused` verdict.
5. Run `target/debug/server --migrate` once. The migration uses an advisory lock;
   `MIGRATION_LOCK_TIMEOUT_SECONDS` defaults to 5 and must be positive. Inspect
   the resulting report. Run preflight again and expect `up_to_date`.
6. Start 0.4 with `SCHEMA_CHANGES=refuse`, then verify existing ticket reads and
   ordinary ticket actions. Start only 0.4 consumers. Their replay checkpoints
   and recovery behavior are described in [consumer-recovery.md](consumer-recovery.md).
7. If validation fails and rollback is needed, stop the 0.4 processes and restore
   the pre-upgrade database and corresponding keys. Run 0.3 on that restore.
   Never try to undo migration 0010 with ad hoc SQL or run 0.3 on the new schema.

The default application pool behavior still retains SQLx client buffers.
The [current performance report](performance-baseline-2026-11-14.md) describes
measured resource usage separately from migration correctness.
