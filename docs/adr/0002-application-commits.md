# ADR 0002: Application commits define history

- Status: proposed
- Date: 2026-08-04

## Context

SurrealKV can optionally retain temporal versions, and SurrealDB may expose database change
features. SurrealFS nevertheless needs semantic history: parent commits, branches, author spans,
mutation sets, messages, policy decisions, artifacts, stable state roots, retention, and export.

Storage-engine versions do not provide that complete domain contract and can change behavior or
retention across engine releases.

## Decision

SurrealFS stores explicit immutable application commits and ordered mutations. Branches and named
snapshots point to those commits. State materialization is defined by the application commit graph.
SurrealKV temporal versioning is optional operational support and can be disabled without changing
checkout, fork, diff, merge, provenance, or logical export.

## Consequences

- History is portable across storage adapters and export formats.
- Retention and GC operate on product-level roots.
- Commits carry semantic causality and can be signed/verified.
- Storage cost may exceed an overwrite-only design.
- Efficient historical reads require indexes/materialized state/checkpoints maintained by the
  application.
- Engine versioning must not be enabled indefinitely by accident.

## Invariant

For every retained commit, SurrealFS can materialize the repository state and recompute the stored
state root using only logical SurrealFS records and chunks. No engine-internal historical API is
required.
