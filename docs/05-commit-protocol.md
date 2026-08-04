# Commit protocol and consistency

## Purpose

The commit protocol is the core trust boundary. It converts a validated domain plan into one visible,
attributable state transition. Performance work may batch or pipeline parts of the protocol, but it
must not weaken the externally visible invariants.

## Commit request

A logical request contains:

```text
CommitRequest {
  protocol_version
  repository_id
  branch_id
  expected_head
  request_id
  request_digest
  cause_id
  run_id?
  session_id
  durability_mode
  mutations[]
  staged_chunk_ids[]
  artifact_declarations[]
  read_set_summary?
  policy_evidence[]
  client_observations
}
```

The request digest covers every field that can change the result, excluding transport-only metadata.
The daemon recomputes the digest from canonical encoding.

## Plan construction

The semantic kernel builds a `CommitPlan` before entering the write transaction:

1. Resolve repository, branch, session, and cause.
2. Read the branch head and required current state using a consistent view.
3. Validate permissions, policy, quotas, and operation preconditions.
4. Apply requested operations to a pure in-memory model of affected objects.
5. Derive ordered mutations, current-state updates, history versions, tombstones, relations, and
   artifact manifests.
6. Verify all referenced staged chunks exist and match declared metadata.
7. Compute canonical mutation root and candidate commit ID.
8. Record the expected branch head used to compute the plan.

A plan is immutable. If the expected head changes, the plan is discarded or explicitly rebased by
the kernel; it is never silently applied to the new head.

## Chunk staging protocol

Large byte streams should not be buffered inside the final metadata transaction.

### Stage

1. Split bytes using the selected versioned chunking algorithm.
2. Compute BLAKE3 on uncompressed bytes.
3. Check whether each chunk record already exists.
4. For missing chunks, write immutable `staged` records idempotently.
5. Read back or otherwise verify length and digest according to the verification policy.

Staging does not change visible file or artifact state.

### Reference

The metadata commit creates extents/manifests referencing staged chunks. Once referenced by a
committed reachable record, a chunk is live. Updating a mutable reference counter is optional and
repairable; reachability remains authoritative.

### Cleanup

Unreferenced staged chunks older than the maximum retry/import window become GC candidates. GC first
rechecks reachability in a consistent view and records progress so interruption is safe.

## SurrealDB transaction algorithm

The adapter performs the following in one explicit transaction:

1. **Idempotency lookup.** Read `request_receipt` by repository/request ID.
   - If digest matches and receipt is complete, return the original result.
   - If digest differs, abort with `IdempotencyKeyReused`.
2. **Branch fence.** Read branch record and require `head == expected_head` and writable status.
3. **Dependency validation.** Require repository, cause, parents, chunks, policies, and referenced
   records to exist in the expected repository and state.
4. **Commit uniqueness.** If deterministic commit ID already exists, require canonical content to
   match; otherwise treat it as corruption or identity collision.
5. **Immutable writes.** Create commit, mutation, state-version, artifact, manifest, and evidence
   records. No overwrite semantics are allowed for immutable IDs.
6. **Current-state writes.** Create/update/delete/tombstone materialized inode, dentry, extent, xattr,
   and KV records exactly as the plan specifies.
7. **Relations.** Create deterministic causal and provenance relation records.
8. **Branch advancement.** Set head to new commit, increment sequence/generation, and retain previous
   head in commit parents.
9. **Receipt.** Create completed request receipt containing commit and response digest.
10. **Commit.** Complete the transaction using the selected durability mode.

No response is successful before step 10.

## Expected-head conflicts

The public conflict response contains:

- requested expected head;
- observed head;
- branch generation;
- whether the request ID was previously completed;
- a stable error code;
- optional safe retry guidance.

The client may:

- reread and recompute;
- create a fork from the expected head;
- invoke an explicit rebase/merge operation;
- abandon the request.

Blind last-writer-wins behavior is forbidden for filesystem namespace and branch updates.

## Idempotency

Every mutating RPC requires a request ID generated before the first attempt. Retries across client
reconnect and daemon restart must return the same logical receipt.

Idempotency is not implemented by guessing from commit content alone. The receipt binds request ID,
request digest, commit, and response.

Special cases:

- Chunk staging is independently idempotent by content hash.
- Start/end span operations have their own request IDs and deterministic framework invocation IDs.
- Imports retain source identity to avoid duplicate genesis or batch application.
- A timeout after server commit is resolved by receipt lookup, not by replaying blindly.

## Cause and span lifecycle

A tool call can produce multiple commits:

```text
tool span starts
  -> commit A
  -> commit B
  -> external observation
  -> commit C
tool span ends with result/error
```

Each commit directly references the span. The span status can be completed later. A failed or
interrupted span does not invalidate commits already acknowledged; it records that they occurred
before failure.

Starting and completing a span are themselves durable events when audit guarantees require them.
For lower-overhead modes, start may be buffered but a commit must ensure its cause record exists in
the same transaction.

## Read sets

Read capture has levels:

- `none`: no dependency claim;
- `semantic`: SDK/tool declares meaningful inputs;
- `filesystem`: kernel records path/inode/content observations;
- `strict`: sandbox/syscall layer records supported accesses;
- `summarized`: high-volume reads are grouped into a signed/digested set record.

A commit records its capture level. Product queries must never present semantic capture as complete
syscall evidence.

## External side effects

Database atomicity cannot roll back an email, network mutation, payment, deployment, or human action.
Tool definitions declare side-effect class:

- pure/read-only;
- idempotent external write;
- compensatable write;
- irreversible/non-idempotent write;
- unknown.

The execution graph records external intent, attempt identity, response/evidence, and compensation.
For idempotent services, propagate the SurrealFS request ID. For irreversible actions, policy may
require approval before execution. SurrealFS must not claim distributed atomicity it does not own.

## Durability modes

### Durable

- SurrealKV sync mode `every`;
- commit returns only after the adapter's grouped sync boundary completes;
- production default;
- receipt is durable with commit.

### Balanced

- bounded background sync interval;
- UI and API disclose the maximum expected loss window;
- suitable for low-value iterative state, not audit-grade evidence.

### Ephemeral

- OS-buffered or intentionally disposable operation;
- explicitly selected per repository/session;
- never labeled durable;
- useful for benchmarks and throwaway sandboxes.

Changing mode affects future acknowledgements, not the historical label on prior commits.

## Crash matrix

| Failure point | Required recovery result |
|---|---|
| Before chunk staging | No visible mutation |
| During chunk staging | Complete immutable chunks or reclaimable partial/unreferenced data |
| After staging, before metadata transaction | No file/artifact reference; chunks retryable/GC-able |
| During metadata transaction | Entire transaction absent after recovery |
| After DB commit, before client response | Receipt lookup returns committed result |
| During durability sync | Either prior durable head or complete new commit; never partial graph/state |
| After response in durable mode | New commit and receipt recoverable |
| During span completion | Existing commits retain cause; span may recover as interrupted |
| During GC | Reachable content retained; GC resumable |
| During migration | Store opens in declared old/in-progress/new state, never ambiguous |

## State-root computation

The commit transaction stores a state root only when it can be computed safely within the latency
budget. The initial implementation may use:

- mutation root synchronously;
- state root synchronously for small repositories;
- asynchronously verified state root with an explicit `pending/verified/failed` status for large
  repositories.

A pending root cannot be used as proof until verified. Root verification failure quarantines the
repository or commit and emits a high-severity integrity event.

## Recovery procedure

On open:

1. Let SurrealKV recover its storage structures.
2. Confirm schema and migration state.
3. Read repository and last branch-head records.
4. Verify every branch head resolves to a complete commit.
5. Check complete receipts reference complete commits.
6. Reconcile interrupted spans and sessions.
7. Resume bounded chunk verification/GC/migration jobs.
8. Optionally verify recent commit mutation roots and sampled content.
9. Enter writable service only if required invariants pass.

Repair tools produce a report and explicit plan. They do not silently discard committed records.

## Transaction-size controls

Bound:

- mutations per commit;
- serialized transaction bytes;
- current/history records touched;
- relation count;
- directory rename subtree work;
- artifact manifest size;
- read-set detail.

Operations exceeding limits use staged plans or multiple commits with explicit batch/workflow records.
Partial commits remain visible as their own states; the UI must represent batch progress honestly.

