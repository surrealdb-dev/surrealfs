# System architecture

## Architectural objective

SurrealFS must make one Rust implementation authoritative for transactional-workspace, filesystem,
KV, immutable-root, history, causality, and publication semantics. The canonical adapter uses
SurrealDB for records, graph queries, indexes, and transactions over SurrealKV. Other languages and
mount protocols are clients or adapters; they are not alternate implementations of the state
machine. The domain contract remains testable against a pure reference model. No second storage
adapter or AgentFS extension is part of the implementation plan.

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
- mints and validates scoped workspace capabilities;
- launches or binds controlled process trees to private workspace overlays;
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
- immutable state-node/root writes and optional head-projection writes;
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

The direct SDK/sandbox workspace is the reference write path. General shared mounts are later
adapters and cannot redefine publication on syscall, `close`, or `fsync`. A platform that cannot
enforce the workspace/process boundary ships with reduced, explicit guarantees or read-only support.

### SDKs

SDKs provide ergonomic async and sync APIs, type conversion, local daemon discovery, reconnect, and
streaming. All SDKs run the same protocol conformance fixtures.

## Write path

### Transactional workspace publication

```text
daemon opens workspace at expected branch head
  -> mint scoped capability and process boundary
  -> tool/descendants stage file + KV + artifact changes privately
  -> wait for the configured process tree to become quiescent
  -> authenticate publish and revalidate capability/span/base head
  -> kernel applies workspace delta to immutable base root
  -> kernel builds CommitPlan with new state root
  -> store begins SurrealDB transaction
  -> check request id and expected branch head
  -> write immutable state nodes, root, history, and commit records
  -> optionally update disposable materialized-head projections
  -> create causal relations
  -> advance branch head and store request receipt
  -> commit at configured durability boundary
  -> return CommitReceipt
```

### Streaming file write

```text
open private workspace write stream
  -> stream bytes through chunker
  -> hash and stage missing chunks idempotently
  -> update workspace-local extent delta
  -> on explicit publish, path-copy extent/inode/namespace roots
  -> commit root, artifact, causality, branch head, and receipt
  -> mark chunks reachable through committed records
```

Staged chunks are not visible as file state. A failed metadata commit can leave unreferenced chunks;
garbage collection removes them after the retry and recovery window.

## Read path

Current and historical reads resolve a branch or `CommitRef` to that commit's immutable state root.
Lookup walks the versioned persistent tree; it never scans ancestry to find the latest applicable
version. Optional materialized-head projections may accelerate current reads after measurement, but
each projection identifies its source root and can be deleted/rebuilt without losing truth. Named
snapshots and commits provide stable anchors for caching.

Content reads fetch extents in file-offset order, load chunks, verify content hashes according to the
configured verification policy, and stream requested ranges to the client.

## Concurrency model

The daemon may process reads concurrently. Each writable tool gets a separate workspace based on one
commit. Workspace changes are isolated until publication. Publishes to one branch use optimistic
concurrency against the branch head:

- every commit request includes `expected_head`;
- the transaction verifies the current head;
- a concurrent change produces a typed conflict;
- the caller may recompute against the new head or create a branch.

Different branches can commit concurrently when their records and shared uniqueness constraints do
not conflict. The kernel must never resolve a conflict by silently applying a stale plan.

The Linux proof forbids detached descendants and rejects or serializes nested writable workspaces.
Observational child spans may share their parent's workspace. General nested/concurrent write
semantics require a later explicit ADR and cannot emerge accidentally from mount behavior.

The initial implementation should limit transaction concurrency until measurements justify broader
parallelism. Predictable correctness is more valuable than speculative concurrency in the first
vertical slice.

## Data placement

| Data | Placement | Reason |
|---|---|---|
| Immutable filesystem/KV state roots and nodes | SurrealDB normal tables | Canonical current and historical lookup |
| Optional branch-head projections | SurrealDB projection tables | Disposable measured read acceleration |
| Mutation/history evidence | SurrealDB history tables | Explicit branch/commit semantics and explanation |
| Commits and mutations | SurrealDB normal tables | Canonical audit and reconstruction |
| Causal relations | SurrealDB relation tables | Native graph traversal |
| File and artifact chunks | SurrealDB chunk records / SurrealKV VLog | One local store and content deduplication |
| Workspace capabilities, open handles, transient locks | Daemon memory plus hashed workspace metadata | Process-scoped authority; no bearer secret at rest |
| Workspace file/KV deltas | Private overlay/staging area | Hidden until explicit publish; discardable on abort |
| Logical exports | Portable archive outside database | Engine-independent recovery and interchange |
| Metrics | Telemetry pipeline, optionally summarized in DB | Avoid hot metrics writes contaminating commit path |

## Deployment modes

### Local embedded mode — required

One daemon embeds the selected store and owns its directory. A controlled Linux SDK/sandbox workspace
is the correctness reference and first proof target. Mounts connect later through the same domain
protocol after their visibility and attribution guarantees are demonstrated.

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
3. Abort or explicitly reconcile unpublished workspaces; never auto-publish them.
4. Stop GC and background migration work.
5. flush/sync according to durability policy;
6. close the embedded database;
7. release store ownership.

This is the required product lifecycle, not a claim that the current public SDK already exposes
each step as an awaited call. The current SDK initiates internal shutdown when all route senders are
dropped. Phase 0 must prove drop/drain/reopen and lock release or obtain a supported public awaited
shutdown capability. A graceful path improves operations; acknowledged-commit safety remains valid
under process death without it.

## Failure boundaries

- **Client failure:** idempotency key permits safe retry.
- **Workspace/tool failure:** unpublished file/KV state remains private and is discarded after the
  recovery window; no branch moves.
- **Background descendant:** publish waits for quiescence; policy timeout kills/aborts the workspace.
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
- workspace open/publish/abort latency, conflicts, expired workspaces, and rejected attribution;
- process-tree quiescence time and detected bypass attempts;
- SurrealKV sync latency, cache use, memtable/VLog growth, and reopen time;
- root-node/projection/history/chunk byte estimates;
- tree depth, node fanout, subtree sharing, and historical-read cost;
- graph-query latency and scanned record counts;
- background GC, migration, backup, and export progress;
- invariant and integrity-check failures.

Metrics must not contain source code, secrets, raw prompts, or file contents by default.
