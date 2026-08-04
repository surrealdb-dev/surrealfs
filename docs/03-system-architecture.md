# System architecture

## Architectural objective

SurrealFS must make one Rust implementation authoritative for filesystem semantics, KV semantics,
history, causality, and storage transitions. SurrealDB provides the record, graph, query, index, and
transaction layer. SurrealKV provides embedded durable storage. Other languages and mount protocols
are clients or adapters; they are not alternate implementations of the state machine.

## Context

```text
                        +--------------------------+
                        | Hosted/team control plane|
                        | optional replication, UI |
                        +------------+-------------+
                                     |
                                     | logical events/exports
                                     |
+--------------+     local RPC     +-v-------------------------------+
| Agent SDKs   +------------------>|                                 |
+--------------+                   |            surrealfsd            |
+--------------+  filesystem ops   |                                 |
| FUSE / NFS   +------------------>|  auth  sessions  quotas  metrics |
+--------------+                   |             |                    |
+--------------+ process/tool evt  |       semantic kernel            |
| Sandbox      +------------------>|             |                    |
+--------------+                   |        store adapter              |
+--------------+ graph/read API    |             |                    |
| CLI / UI     +------------------>|   embedded SurrealDB / SurrealKV |
+--------------+                   +---------------------------------+
```

The hosted plane is not required for local correctness. It may later receive logical events, signed
exports, summaries, or replicated repositories, but local commits never wait for a hosted service.

## Component responsibilities

### `surrealfsd`

The daemon owns the database directory and process-scoped state:

- opens and migrates the store;
- enforces one-writer ownership;
- accepts authenticated local connections;
- manages repository, branch, and session handles;
- maintains open file handles and advisory locks;
- applies quotas, rate limits, cancellation, and backpressure;
- exposes health, metrics, backup, export, and administrative operations;
- performs graceful shutdown and verifies the durability boundary.

No client process opens the embedded database directory independently.

### Semantic kernel

The kernel accepts domain commands rather than arbitrary records:

- filesystem operations expressed through inode and parent/name identities;
- KV operations with optional conditional checks;
- run and span lifecycle events;
- commit, branch, snapshot, diff, and merge commands;
- artifact and evaluation operations;
- policy decisions and enforcement results.

It validates preconditions, derives mutations, computes identities and hashes, and creates one
`CommitPlan`. It should be testable against an in-memory reference model without SurrealDB.

### SurrealDB store adapter

The adapter translates a validated `CommitPlan` into parameterized SurrealQL or typed SDK operations.
It owns:

- schema migrations;
- deterministic record-ID encoding;
- transaction construction;
- expected-head conflict checks;
- idempotency checks;
- current-state and history writes;
- relation creation;
- query plans and index expectations;
- conversion from database errors to domain errors.

It must not contain filesystem policy that belongs in the kernel.

The adapter depends only on the public `surrealdb` Rust SDK with the `kv-surrealkv` feature. It does
not import `engine-api`, `engine-local`, `datastore`, `kvs`, `kvs-any`, or `kvs-surrealkv`, and it
does not use the hidden `unstable_from_datastore` constructor. Those current private-tree packages
are internal implementation details without a stability guarantee.

### Content subsystem

The content subsystem chunks streams, hashes bytes, compresses where useful, stages immutable chunks,
maps file ranges to chunks, verifies reads, and reclaims unreachable chunks after a safety period.

The first implementation stores chunks as SurrealDB records backed by SurrealKV's value log. It does
not create one graph relation per chunk; file extents and artifact manifests reference chunk record
IDs as fields. This prevents content plumbing from overwhelming the causal graph.

### Protocol layer

The local protocol is a versioned binary RPC transported over Unix-domain sockets on Unix and an
equivalent protected local transport on Windows. It provides multiplexing, streaming, cancellation,
flow control, feature negotiation, and structured errors.

Protocol messages use domain values. They do not expose SurrealDB values or physical record layouts.

### Mount adapters

FUSE and NFS translate protocol-specific requests into kernel commands. They own request/reply
translation and cache invalidation behavior, not storage or filesystem truth.

### SDKs

SDKs provide ergonomic async and sync APIs, type conversion, local daemon discovery, reconnect, and
streaming. All SDKs run the same protocol conformance fixtures.

## Write path

### Small semantic mutation

```text
client request
  -> authenticate and resolve session/repository/branch
  -> kernel reads required current records
  -> kernel validates filesystem/KV/span invariants
  -> kernel builds CommitPlan
  -> store begins SurrealDB transaction
  -> check request id and expected branch head
  -> write immutable history and commit records
  -> update materialized current records
  -> create causal relations
  -> advance branch head and store request receipt
  -> commit at configured durability boundary
  -> return CommitReceipt
```

### Streaming file write

```text
open write session
  -> stream bytes through chunker
  -> hash and stage missing chunks idempotently
  -> build extent replacement mutation
  -> commit extent, inode, artifact, causality, and branch head
  -> mark chunks reachable through committed records
```

Staged chunks are not visible as file state. A failed metadata commit can leave unreferenced chunks;
garbage collection removes them after the retry and recovery window.

## Read path

Current reads use materialized branch-head records. A filesystem lookup should not reconstruct the
entire branch history.

Historical reads resolve a `CommitRef` to branch and sequence, then select the latest applicable
immutable state version. The first implementation may materialize a read view for repeated historical
access. Named snapshots and branch bases provide stable anchors for caching.

Content reads fetch extents in file-offset order, load chunks, verify content hashes according to the
configured verification policy, and stream requested ranges to the client.

## Concurrency model

The daemon may process reads concurrently. Writes to one branch use optimistic concurrency against
the branch head:

- every commit request includes `expected_head`;
- the transaction verifies the current head;
- a concurrent change produces a typed conflict;
- the caller may recompute against the new head or create a branch.

Different branches can commit concurrently when their records and shared uniqueness constraints do
not conflict. The kernel must never resolve a conflict by silently applying a stale plan.

The initial implementation should limit transaction concurrency until measurements justify broader
parallelism. Predictable correctness is more valuable than speculative concurrency in the first
vertical slice.

## Data placement

| Data | Placement | Reason |
|---|---|---|
| Current filesystem and KV state | SurrealDB normal tables | Fast record/range reads and indexes |
| Immutable state history | SurrealDB history tables | Explicit branch/commit semantics |
| Commits and mutations | SurrealDB normal tables | Canonical audit and reconstruction |
| Causal relations | SurrealDB relation tables | Native graph traversal |
| File and artifact chunks | SurrealDB chunk records / SurrealKV VLog | One local store and content deduplication |
| Open handles and transient locks | Daemon memory | Process-scoped semantics; reconstructed safely |
| Logical exports | Portable archive outside database | Engine-independent recovery and interchange |
| Metrics | Telemetry pipeline, optionally summarized in DB | Avoid hot metrics writes contaminating commit path |

## Deployment modes

### Local embedded mode — required

One daemon embeds SurrealDB and opens one SurrealKV directory. SDKs and mounts connect locally. This
is the correctness reference and the first production target.

### Container/CI mode — required

The daemon and sandbox run in a container or VM with a persistent data volume. Shutdown hooks improve
operational behavior, but crash safety cannot depend on them.

### Remote repository service — later

Clients connect to a service that owns the embedded store. Multi-tenant scheduling, encryption,
replication, and failover are service concerns. A single SurrealKV directory is never opened by
multiple independent service processes.

### Browser mode — compatibility, not canonical

Browser SDKs connect to a remote daemon/service. An IndexedDB-only simulation may exist for demos,
but it must declare reduced guarantees and cannot define canonical semantics.

## Lifecycle

### Open

1. Acquire exclusive store ownership.
2. Validate repository metadata and supported engine range.
3. Recover SurrealKV.
4. Apply or resume idempotent schema migrations.
5. Verify critical references and last durable receipt.
6. Start background tasks and accept requests.

### Steady state

- serve reads and commits;
- stage and verify chunks;
- collect metrics;
- expire sessions;
- run bounded GC and retention tasks;
- create backups and logical exports according to policy.

### Shutdown

1. Stop accepting new mutating sessions.
2. Let active requests finish or cancel by deadline.
3. Stop GC and background migration work.
4. flush/sync according to durability policy;
5. close the embedded database;
6. release store ownership.

This is the required product lifecycle, not a claim that the current public SDK already exposes
each step as an awaited call. The current SDK initiates internal shutdown when all route senders are
dropped. Phase 0 must prove drop/drain/reopen and lock release or obtain a supported public awaited
shutdown capability. A graceful path improves operations; acknowledged-commit safety remains valid
under process death without it.

## Failure boundaries

- **Client failure:** idempotency key permits safe retry.
- **Daemon failure:** recovered database contains complete committed transactions only; open handles
  disappear and are reconciled.
- **Chunk-stage failure:** no branch state references incomplete bytes.
- **Metadata transaction failure:** no branch head or causal relation becomes partially visible.
- **Hosted service failure:** local operation continues; pending sync remains retryable.
- **Schema migration failure:** store remains on a declared migration state and refuses unsafe writes.
- **Integrity failure:** affected repository enters read-only/quarantine mode; repair never silently
  invents missing content.

## Observability

At minimum expose:

- request and commit latency histograms;
- commit conflicts and idempotent retry counts;
- chunk staging, deduplication, and verification rates;
- transaction size and mutation count;
- SurrealKV sync latency, cache use, memtable/VLog growth, and reopen time;
- current/history/chunk byte estimates;
- branch depth and historical-read cost;
- graph-query latency and scanned record counts;
- background GC, migration, backup, and export progress;
- invariant and integrity-check failures.

Metrics must not contain source code, secrets, raw prompts, or file contents by default.
