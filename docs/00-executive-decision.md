# Executive decision

## Decision

SurrealFS will use embedded SurrealDB backed by SurrealKV as its one canonical persistence layer.
The first production version will not offer raw SurrealKV as a separate storage mode.

The decision is driven by product fit rather than an assertion that this combination is universally
the fastest embedded datastore. Agent execution naturally produces a graph of runs, actions, state
transitions, artifacts, branches, policies, and evaluations. SurrealDB makes that graph queryable
without building a second index or a custom graph engine. SurrealKV supplies local embedded storage,
transactions, snapshot isolation, range access, versioning support, and a value log.

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
- accumulated execution and outcome data;
- derived recovery, evaluation, and governance capabilities.

## Conditions for proceeding

Proceed with the full reconstruction only after a vertical slice proves all of the following:

1. One SurrealDB transaction can atomically apply current state, immutable history, graph edges, and
   branch-head advancement.
2. The durable mode survives fault injection without losing acknowledged commits.
3. Filesystem metadata operations stay inside an agreed latency envelope.
4. Content-addressed chunks can be streamed without oversized transactions or pathological growth.
5. At least three graph-powered product workflows deliver clear user value.
6. Logical export/import allows recovery from physical-format or dependency changes.
7. SurrealDB licensing and the private-source dependency are acceptable for the intended product.

## Conditions for revisiting the engine choice

Revisit SurrealDB-over-SurrealKV if query execution dominates filesystem cost after schema and access
path optimization, if database upgrades are operationally unsafe, if SurrealKV durability fails the
fault model, or if the graph is not central to product usage. Preserve the domain protocol and
application-level history so an engine change does not require redefining the product.
