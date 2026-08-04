# ADR 0003: One semantic writer owns each database

- Status: proposed
- Date: 2026-08-04

## Context

Direct database access from SDKs, FUSE callbacks, analytics, and agent processes would appear
convenient. It would also allow state mutations without idempotency receipts, expected-head checks,
authorization, policy, ordered mutations, and causal authorship. Multiple embedded processes may
also violate storage-directory ownership assumptions.

## Decision

One `surrealfsd` process exclusively opens a SurrealFS database directory. All clients use a
versioned RPC/domain interface. The daemon may execute concurrent transactions, but every mutation
passes through one semantic kernel. Raw SurrealQL is scoped read-only; unsafe writes require
offline recovery mode and post-write verification.

## Consequences

- provenance completeness can be enforced at the choke point;
- SDKs and mounts share identical semantics;
- authentication, policy, quotas, audit, and idempotency are centralized;
- clients are insulated from schema and engine changes;
- the daemon is a local availability and throughput dependency;
- deployments need process lifecycle, socket security, locking, and fast recovery;
- horizontal multiwriter architecture requires a new ADR rather than silently adding direct
  database access.

## Enforcement

- private database path and exclusive OS lock;
- no client database credentials/handles;
- application tables use no direct client permissions;
- architecture dependency checks prevent SDK/mount crates importing the database adapter;
- invariant scans detect commits or materialized state without required provenance.
