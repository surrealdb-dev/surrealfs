# ADR 0001: SurrealDB + SurrealKV is the preferred canonical-store candidate

- Status: proposed, conditional on Phase 0 gates
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

Use embedded SurrealDB with SurrealKV as the preferred implementation for the bounded proof. Only
`surrealfsd` opens it. All mutations pass through the SurrealFS semantic kernel in one database
transaction. Use an engine-independent domain storage trait and logical export.

Before accepting this ADR for production, implement the same small causal-commit conformance
scenario against a SQLite/AgentFS baseline with equivalent durability. This is an evaluation spike,
not a commitment to maintain two production adapters. If SurrealDB/SurrealKV passes, support it as
the single initial production adapter. If it fails while the domain semantics validate, select the
baseline or another adapter without changing commit identity, state roots, RPC, or export semantics.

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

The decision is accepted only after the parity spike plus transaction concurrency, crash/reopen,
backup/restore, logical export, performance, compaction, security, and license gates in docs 08 and
12 pass. Correctness is pass/fail and precedes performance. The decision report must include adapter
code/complexity, query plans, disk amplification, lifecycle behavior, and the recovery workflow—not
only microbenchmarks. If product semantics validate but the engine fails, replace the adapter without
changing the protocol/domain contract.

The current revalidation found that public SDK transactions, session isolation, live queries, and
logical export/import have targeted passing tests. It also found no supported public awaited
shutdown contract and no proven live SurrealKV physical-snapshot API. Those are Phase 0 gaps, not
reasons to use internal engine/KVS crates. See [the current source audit](../15-current-surrealdb-audit.md).

## Rejected for now

- SurrealKV-only public mode: duplicate engineering and test matrix without a moat benefit.
- RocksDB + SurrealDB projection: unnecessary two-store consistency cost for the chosen local
  architecture.
- SQLite as a second production mode: two permanent backends would expand the compatibility matrix
  before product value is proven. SQLite/AgentFS remains the mandatory Phase 0 evaluation baseline
  and a valid production fallback if it wins.
