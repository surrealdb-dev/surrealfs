# ADR 0001: SurrealDB + SurrealKV is the canonical store

- Status: accepted
- Date: 2026-08-04
- Revalidated: 2026-08-04 against private revision `e68539867`, SurrealKV `0.21.3`

## Context

SurrealFS must atomically bind filesystem/KV state changes to runs, tool calls, artifacts, policy,
and evaluations. Alternatives considered were SQLite, RocksDB alone, SurrealKV alone, a RocksDB
ledger projected into SurrealDB, and SurrealDB embedded over SurrealKV.

A two-store design preserves a simple ledger but requires projection state, idempotency, lag,
rebuild, graph/source consistency rules, backup coordination, and a clear degraded-mode story. Raw
SurrealKV avoids database abstraction overhead but forces the project to implement graph/index/
migration/query machinery that is not user-visible differentiation.

## Decision

Build SurrealFS from scratch using embedded SurrealDB with SurrealKV as one canonical store. Only
`surrealfsd` opens it. All mutations pass through the SurrealFS semantic kernel in one database
transaction. Maintain a database-independent domain model and logical export for testability and
user portability, not to promise alternate engines.

## Consequences

Positive:

- state and provenance can commit atomically;
- record links, graph traversal, indexes, and subscriptions are available without a projection;
- local/offline deployment stays simple;
- one backup and recovery boundary;
- fewer dual-source-of-truth failure modes.

Negative:

- filesystem hot paths pay SurrealDB query/record overhead;
- SurrealKV is publicly described as beta;
- the selected local checkout is a private/nightly line;
- SurrealDB licensing and commercial deployment need legal review;
- a single embedded owner limits distributed/multiwriter deployment;
- engine/schema upgrades become release-critical.

## Validation

Production work proceeds only after transaction concurrency, crash/reopen, backup/restore, logical
export, performance, compaction, security, and license gates in docs 08 and 12 pass. Correctness is
pass/fail and precedes performance. The evidence report includes schema/query complexity, query
plans, disk amplification, lifecycle behavior, and the recovery workflow—not only microbenchmarks.
If the fixed stack fails a critical gate, stop or narrow the product.

The current revalidation found that public SDK transactions, session isolation, live queries, and
logical export/import have targeted passing tests. It also found no supported public awaited
shutdown contract and no proven live SurrealKV physical-snapshot API. Those are Phase 0 gaps, not
reasons to use internal engine/KVS crates. See [the current source audit](../15-current-surrealdb-audit.md).

## Rejected for now

- SurrealKV-only public mode: duplicate engineering and test matrix without a moat benefit.
- RocksDB + SurrealDB projection: unnecessary two-store consistency cost for the chosen local
  architecture.
- SQLite or AgentFS adapter/extension: SurrealFS is a from-scratch implementation on the chosen stack.
