# Executive decision: fund the proof, not the full rewrite

## Decision

Fund a tightly bounded proof of the SurrealFS causal-workspace contract. Use embedded SurrealDB
backed by SurrealKV as the **preferred proof implementation**, not as a production conclusion.
The same minimal `CausalCommitStore` protocol must also be exercised against a SQLite/AgentFS
baseline during Phase 0. Production selection follows correctness, lifecycle, complexity, workload,
and product evidence.

If SurrealDB + SurrealKV passes those gates, it becomes the one canonical persistence layer for the
first production architecture. The first production version will not offer raw SurrealKV as a
separate storage mode. If it fails while the product semantics validate, replace the adapter rather
than weakening the product contract.

The preference is driven by product fit rather than an assertion that this combination is
universally the fastest or safest embedded datastore. Agent execution naturally produces a graph of
runs, actions, state transitions, artifacts, branches, policies, and evaluations. SurrealDB may make
that graph queryable without building a second index or a custom graph engine. SurrealKV supplies the
candidate local embedded storage layer. Phase 0 must prove that this convenience survives the
required durability and hot-path workload.

## Why the earlier two-store design is no longer preferred

An earlier option treated a lower-level ledger as canonical and SurrealDB as an asynchronous graph
projection. That arrangement was appropriate when the canonical engine was RocksDB and SurrealDB was
a networked control-plane dependency. It created costs:

- two schemas and two operational stores;
- an outbox and projection lag;
- eventual rather than atomic agreement between state and provenance;
- rebuild and repair tooling;
- unclear behavior when graph-dependent features run offline.

Embedding SurrealDB over SurrealKV removes the network dependency and allows filesystem state, KV
state, commit records, graph relations, and branch-head advancement to share one transaction.

## Why raw SurrealKV is not the first choice

Raw SurrealKV would provide greater control over key layout and remove query-layer overhead. It would
also require SurrealFS to implement or maintain:

- record encoding and schema validation;
- secondary and uniqueness indexes;
- graph adjacency indexes and traversal;
- query and pagination behavior;
- live subscriptions and resumable change delivery;
- permissions around introspection;
- migration behavior for all of the above.

Those tasks delay the differentiated workflows. A raw engine remains a contingency if benchmarks
prove the SurrealDB layer unsuitable, not a parallel promise that must be supported from day one.

## Evidence from the selected private SurrealDB checkout

The design was revalidated on 2026-08-04 against private source revision `e68539867`
(`v3.3.0-nightly`, SurrealKV `0.21.3`). The current checkout provides:

- a typed SDK-to-engine boundary and a more modular local engine/datastore/KVS architecture;
- a SurrealKV adapter with conditional operations, conflicts, savepoints, snapshots, and range scans;
- public client-side transactions plus local embedded live-query and logical export/import support;
- sync-on-every-commit as the SurrealKV adapter default, implemented with grouped WAL fsync;
- value-log separation enabled by default with a 4 KiB threshold;
- startup datastore version checks and an idempotent migration ledger;
- a shared behavior suite across storage implementations.

On the current revision, the shared SurrealKV KVS suite reported 73 passed, 0 failed, and 8 ignored;
the SurrealKV adapter's own tests reported 3 passed; and targeted public-SDK transaction, session
isolation, live-query, logical export/import, and versioned-operation tests passed. This is useful
dependency evidence, not production certification. Crash, compaction, upgrade, retention,
shutdown/reopen, and realistic AgentFS workload tests remain mandatory. See the
[current source audit](15-current-surrealdb-audit.md).

## Strategic thesis

The database combination is worth adopting only as an accelerator for the causal execution product.
It is not a moat by itself. The durable advantage must come from:

- capture completeness and correctness;
- a domain-specific causal model;
- fork/diff/recovery ergonomics;
- integrations at the actual agent execution boundary;
- optional, permissioned outcome signals where customers explicitly choose to share them;
- derived recovery, evaluation, and governance capabilities.

There is no moat today. The storage engine, graph schema, snapshots, and ontology are not moats by
themselves. Defensibility can form only through enforced mutation-boundary integrations, trustworthy
capture under crashes and subprocesses, repeated recovery workflows, and interoperable evidence.
Permissioned aggregate data is optional upside, not the initial investment thesis.

## Conditions for proceeding

Proceed beyond the proof only after it establishes all of the following:

1. The same bounded causal-commit protocol is implemented and tested on the SurrealDB/SurrealKV
   candidate and the SQLite/AgentFS baseline with normalized durability.
2. A commit references an immutable filesystem/KV state root; branch creation from a retained commit
   does not copy the complete state; any materialized head is disposable and rebuildable.
3. One transaction atomically creates immutable state, commit evidence, graph edges, retry receipt,
   and the expected-head branch movement.
4. A private transactional workspace hides staged file/KV changes until explicit publish and
   discards them on abort.
5. Attribution is enforced by a daemon-issued workspace capability and process-tree boundary rather
   than trusted from a caller-supplied span ID.
6. Durable mode survives fault injection without losing or partially exposing acknowledged commits.
7. Logical export/import reconstructs every retained state root independently of physical storage.
8. A real failed-run recovery/fork/compare trial shows repeated user value before broad POSIX work.
9. SurrealDB licensing and the private-source dependency are acceptable for the intended product.

## Conditions for revisiting the engine choice

Choose or revisit SurrealDB-over-SurrealKV from the Phase 0 comparison. Prefer the SQLite/AgentFS
path if it provides equivalent semantics with lower lifecycle, durability, or implementation risk.
Prefer SurrealDB/SurrealKV only if it passes every correctness gate and materially reduces domain
indexing/query complexity or enables the target workflows. Re-open the choice if query execution
dominates filesystem cost, upgrades are operationally unsafe, SurrealKV durability fails the fault
model, or graph-powered workflows are not central to usage. Preserve the domain protocol and
application-level history so an engine change never requires redefining the product.
