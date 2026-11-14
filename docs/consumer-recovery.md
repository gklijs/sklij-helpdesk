# Application consumer recovery (#100)

`alerter`, `engagement-watcher`, `provisioner` and `lifecycle-replicator`
fetch `GET /v1/events?after=<nextCursor>`. Fetching does **not** advance a
server cursor. Opaque, epoch-qualified cursors are saved locally after
processing, together with any state derived from the events. One instance
owns each checkpoint; concurrent writers must use distinct files.

Default files, configurable through environment variables:

| Binary | Variable | Default |
|---|---|---|
| alerter | `ALERTER_STATE_FILE` | `alerter-state.json` |
| engagement-watcher | `ENGAGEMENT_WATCHER_STATE_FILE` | `engagement-watcher-state.json` |
| provisioner | `PROVISIONER_STATE_FILE` | `provisioner-state.json` |
| lifecycle-replicator | `REPLICATOR_STATE_FILE` | `replicator-state.json` |

Use a persistent, writable local volume. Files contain operational identifiers
and positions, never command credentials. Writes use an owner-only temporary
file, fsync, atomic rename, and directory fsync. Failure to read an existing
file, decode it, or checkpoint stops the process. Startup verifies writable
storage before processing. The two stateful watchers retain their explicit
empty-path opt-out; that mode replays history on every restart.

Missing checkpoints rebuild safely from event history, independently of any
old `mode=auto` cursor. Pre-upgrade watcher files without replay positions
also rebuild. Back up the database, encryption key file and checkpoints from
a consistent deployment. Retain the event history needed for reconstruction.

## Delivery and ordering

Delivery is at least once. A crash after delivery, after a command, or before
checkpoint replacement replays work. Domain guards prevent duplicate tenant
records, lifecycle facts, escalations and engagement declines. Provisioning
recognizes a tenant created before its mapping was recorded. Lifecycle
mirroring retries an unprovisioned company without waiting for another
business event; duplicate and regressive states are terminal no-ops.

The alerter records the last lifecycle sequence per ticket, so separate
created/resolved/reopened/closed/merged feeds cannot overwrite a newer state
when catching up. Tracking keys include the context, so identical ticket IDs
in different tenants cannot overwrite each other. Watchers defer deadline sweeps while any feed returns a
full page, using this application's default 1,000-event page cap. Tenant
positions use context/event names and survive token re-minting.

Slack and console alerts are best effort. Urgent alerts can be duplicated
on replay. A crash after escalation commits and before Slack delivery can
lose that external alert. Failed Slack requests are logged without a durable
outbox. These are separate from the durable domain-effect guarantees.

## Recovery

1. Stop the affected consumer.
2. Fix permissions/storage if a checkpoint write failed. Restore a known-good
   checkpoint, or deliberately remove its JSON file to rebuild from history.
3. Restart with valid tokens for the current database and persistent storage.
4. Check logs and the relevant domain projection before returning to service.

A rejected token or cursor (HTTP 400/401/403/409, including an epoch mismatch)
exits visibly. After a database restore, restore matching checkpoints or remove
them to replay the restored history. Deleting a checkpoint needs no new signup,
activity, or lifecycle event to recover pending work.

`tests/consumer_recovery.rs` starts the real binaries and forces exit code 86
after delivery and after domain effects. Provisioning also exits after tenant
creation, before recording it. Restarts and removed checkpoints must converge
without duplicate domain effects. `HELPDESK_CONSUMER_FAULT` selects these test
windows (`after_delivery`, `after_effect`, `after_tenant_creation`) only in the
disposable child process; leave it unset for normal operation.
