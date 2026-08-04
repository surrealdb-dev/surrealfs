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
  workspace_id
  workspace_capability_proof
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

The request digest covers every field that can change the result, excluding transport-only metadata
and the raw bearer capability. It includes the capability identifier/hash and verified process-scope
evidence. The daemon recomputes the digest from canonical encoding.

## Plan construction

The semantic kernel builds a `CommitPlan` before entering the write transaction:

1. Resolve repository, branch, workspace, session, and cause.
2. Verify the opaque capability, author span, process scope, expiry, workspace status, and base commit.
3. Read the branch head and immutable base state root using a consistent view.
4. Validate permissions, policy, quotas, quiescence, and operation preconditions.
5. Apply the workspace delta to a pure persistent-tree reference model.
6. Derive ordered mutations, new immutable nodes/root, explanation records, relations, and manifests.
7. Verify all referenced staged chunks exist and match declared metadata.
8. Compute canonical mutation root, state root, and candidate commit ID.
9. Record the expected branch head used to compute the plan.

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
3. **Workspace/attribution validation.** Require the workspace to be open, capability hash and
   process scope verified, author/cause bound, descendants quiescent, and base equal to expected head.
4. **Dependency validation.** Require repository, cause, parents, chunks, policies, and referenced
   records to exist in the expected repository and state.
5. **Commit uniqueness.** If deterministic commit ID already exists, require canonical content to
   match; otherwise treat it as corruption or identity collision.
6. **Immutable writes.** Create persistent state nodes/root, commit, mutation, artifact, manifest,
   and evidence records. No overwrite semantics are allowed for immutable IDs.
7. **Optional projection writes.** Update a disposable root-keyed head projection only when enabled.
8. **Relations.** Create deterministic causal and provenance relation records.
9. **Workspace closure.** Mark the workspace committed and bind its final commit/root.
10. **Branch advancement.** Set head to new commit, increment sequence/generation, and retain previous
   head in commit parents.
11. **Receipt.** Create completed request receipt containing commit, root, and response digest.
12. **Commit.** Complete the transaction using the selected durability mode.

No response is successful before step 12.

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

## Workspace and span lifecycle

The initial contract maps one writable tool span to one workspace and one publish decision:

```text
tool span starts
  -> open capability-bound workspace at commit H
  -> stage file/KV/artifact changes and external-effect evidence
  -> wait for approved descendants
  -> success: publish one commit H' and finish span
  -> error/cancel/timeout: abort workspace and finish span without internal state publication
```

An explicit checkpoint ends one workspace and publishes one commit; continued work starts a new
workspace, preferably under a child/checkpoint span. This permits a broader span to relate to several
commits without silently turning syscalls into semantic history. A failure never invalidates an
earlier explicit checkpoint, but uncheckpointed workspace state aborts.

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

The canonical cross-boundary protocol, state machine, recovery grades, and proof criteria are defined
in [External effects and recovery](16-external-effects-and-recovery.md). This section summarizes the
commit-boundary rule.

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

Every publish computes the new persistent state root before entering the final transaction. The plan
contains canonical bytes/digests for every newly created path node and the complete root. The
transaction creates missing immutable nodes idempotently and commits the root with evidence, branch,
and receipt. No acknowledged commit may contain a pending or absent root.

Post-commit verification can be asynchronous for cost control. Root verification failure quarantines
the repository or commit and emits a high-severity integrity event; it does not retroactively invent
another root.

## Recovery procedure

On open:

1. Let SurrealKV recover its storage structures.
2. Confirm schema and migration state.
3. Read repository and last branch-head records.
4. Verify every branch head resolves to a complete commit and immutable state root.
5. Check complete receipts reference the same commit/root as their branch outcome.
6. Abort/reconcile interrupted unpublished workspaces and spans; never auto-publish them.
7. Resume bounded chunk verification/GC/migration jobs.
8. Optionally verify recent commit mutation roots and sampled content.
9. Enter writable service only if required invariants pass.

Repair tools produce a report and explicit plan. They do not silently discard committed records.

## Transaction-size controls

Bound:

- mutations per commit;
- serialized transaction bytes;
- immutable nodes, explanation records, and optional projections touched;
- relation count;
- directory rename subtree work;
- artifact manifest size;
- read-set detail.

An operation that cannot fit one publication transaction is rejected or redesigned to use hidden,
idempotent staging plus one bounded final root/branch transaction. Multiple visible commits are allowed
only as explicit user-visible checkpoints/workflow steps, never as an implementation detail falsely
presented as one atomic tool action.
