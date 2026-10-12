# REST vs GraphQL: intended differences

`tests/rest_graphql_conformance.rs` drives one helpdesk scenario (signup,
two tickets, assign, internal note, info request, customer reply,
escalate, merge, resolve, CSAT, and a second rating that gets rejected)
once over REST (`POST /v1/commands/trigger`) and once over GraphQL
(`submitCommand`), each into a database of its own. It then compares
the stored events, the commands behind them, every projection row and
every snapshot row. The normalised result is checked in as
`tests/golden/rest_graphql_conformance.json`. Regenerate it with
`UPDATE_GOLDEN=1 cargo test --test rest_graphql_conformance` and review
the diff.

Only the differences listed here are allowed. Anything else that
differs between the two runs fails the test. To allow a new difference,
add it to this list first and then normalise it in the test.

## Normalised in the transcript

| What | REST | GraphQL | How the test handles it |
|---|---|---|---|
| `metadata.client_id` (command and its events) | the `CommandToken`'s id | the calling Role's id | Left out of the transcript. `check_provenance` asserts it is the submitter's token or Role id. |
| `metadata.correlation_id` | generated per command unless the body passes `correlationId` | generated per command unless the mutation passes `correlationId` | Left out. Checked to be present, shared by a command and its events, and unique per command. |
| `metadata.causation_id` | event: its command's id; command: `None` unless `causationId` is passed | same | Left out. Checked to equal the command id on events and to be `None` on commands. |
| `metadata.created_at`, `Command.id` | wall clock / random | wall clock / random | Left out. |
| Event `sequence`, `consistency_boundary`, projection/snapshot `as_of_sequence` | each database's own numbering | same | Replaced by the event's position in the run (`#1`, `#2`, ...). |
| Sensitive payload fields (`customer` subject) | AES-GCM ciphertext with a fresh nonce | same | Decrypted with the test master key, so the transcript holds plaintext. |
| Async projection and `TicketSnapshot` rows | written by background catch-up, so they can lag | same | Not a surface difference. The test waits until they have folded the last event before reading them. |
| `triggeredEventSequences` in the answer | sequence numbers | same | Only the count is kept. |

## Surface differences the transcript does not capture

- **Credentials and who may submit.** REST takes a `CommandToken` minted
  for one command type from a `RoleAccessMapping`. The token is its own
  per-command grant, so `#[requires_role("staff")]` is not applied over
  REST. GraphQL takes an IdP-issued JWT for a Role: the role gate
  applies (`insufficient_role`), and so does `RoleAccessMapping.scope`.
  That is why the scenario submits everything as one admin over REST,
  but as the right Role over GraphQL: an admin for signup, `staff` for
  staff commands, and a `customer` Role scoped to the company for
  customer commands.
- **REST is opt-in per command type.** A command type is triggerable
  over REST only if it sets `rest_trigger_allowed()`. GraphQL accepts
  any command type for a write-level grant. Every helpdesk/activity
  command a person submits opts in. The three marketing commands that
  only cross-context routes submit (`RecordTrialConversion`,
  `RecordTrialLapse`, `RecordEngagementDecline`) do not.
- **Payload encoding.** REST takes the payload as typed JSON
  (`{"payload": {...}}`). GraphQL's `payload: String!` is a JSON string
  (see `support::submit_command_mutation`).
- **Answer shape.** Both answer with HTTP 200 and the same
  `accepted`/`rejectionKind`/`rejectionReason`/`triggeredEventSequences`
  fields. REST omits absent fields, while GraphQL returns them as
  `null`. A GraphQL rejection can also carry `matchingEvents` for an
  admin-level caller. REST additionally returns `deduplicated` and
  `correlationId`. A refused credential is an HTTP 401/403 over REST
  and a GraphQL error with `extensions.code` over GraphQL.
- **Idempotency.** REST uses an `Idempotency-Key` header and GraphQL
  uses an `idempotencyKey` argument. The scenario uses neither.
- **Reads.** Projections are read over GraphQL (`projection(...)`,
  including `waitForSequence`). REST reads events
  (`/v1/events`, `/v1/events/consume`), not projections. The test compares projection
  rows in the database, so it covers what both surfaces wrote, not how
  each one serves reads.
