# ADR 0005: transactional workspaces define publication and attribution

- Status: accepted for the Linux proof
- Date: 2026-08-04

## Context

Per-syscall commits reproduce low-level operation logs rather than semantic agent actions. Publishing
on `close` or `fsync` cannot reliably group all file, KV, artifact, and subprocess effects caused by
one tool. Trusting a caller-supplied span ID also proves only declared attribution: a child process,
detached background task, nested tool, or bypass path can write under the wrong identity.

## Decision

The first product boundary is an explicit transactional workspace:

- a tool and approved descendants see a private file overlay and KV/artifact delta;
- other sessions continue to see the last committed head;
- `close` and `fsync` affect the private workspace but do not publish it;
- explicit `publish(expected_head, request_id)` creates one causal commit;
- abort discards the workspace without a visible state transition.

`surrealfsd` creates an opaque workspace capability and binds it to a repository, base commit,
principal, author span, process scope, expiry, and permissions. The capability is inherited only
through the controlled launch path and is never derived from W3C/OpenTelemetry trace identifiers.
Trace context remains interoperable correlation metadata, not authorization.

The Linux proof gives each writable tool a mount namespace, cgroup subtree, overlay upper/work
directory, and access to the daemon workspace endpoint. The database directory and committed lower
state are not directly writable. Publication waits for the defined process tree to become quiescent.

Initial restrictions are deliberate:

- missing or invalid capability rejects the write;
- concurrent writable tools use separate workspaces and expected-head conflicts;
- nested writable tools are rejected or serialized; observational child spans may share a workspace;
- detached/background descendants are forbidden; timeout kills the workspace process tree and aborts;
- system/import/maintenance commits use explicit non-tool cause classes and never masquerade as
  fully captured tool attribution.

## Consequences

- the direct SDK/sandbox path is the first supported write surface;
- broad FUSE/NFS and macOS attribution parity are deferred until demanded and proven;
- tools cannot observe another workspace's uncommitted writes;
- a failed tool can abort all staged internal state, but external effects still require idempotency,
  compensation, or human reconciliation;
- the threat model and conformance suite include propagation, bypass, concurrency, and process exit.

## Invariants

1. No workspace mutation is visible through a committed branch before publish succeeds.
2. Publish binds exactly one workspace capability, cause span, expected head, state root, and receipt.
3. `CAPTURED` attribution is impossible when capability/process-scope verification is missing.
4. Tool completion is not successful while forbidden descendants remain alive.
5. Abort, timeout, daemon crash before commit, and expected-head conflict expose none of the staged
   logical state.
