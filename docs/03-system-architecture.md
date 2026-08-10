# System architecture

## Architectural objective

SurrealFS must make one embeddable Rust implementation authoritative for transactional-workspace,
filesystem, KV, immutable-root, history, causality, and publication semantics. The canonical
adapter uses SurrealDB for records, graph queries, indexes, and transactions over SurrealKV. The
public Rust SDK can open that kernel in-process. Mounts, sandboxes, MCP, and an optional service are
adapters or deployment shells; they are not alternate implementations of the state machine. The
domain contract remains testable against a pure reference model. No second storage adapter or
AgentFS extension is part of the implementation plan.

## Context

```text
embedded Rust application                 unmodified tools / remote clients
          |                                             |
          | in-process domain API                       | POSIX / RPC / stdio
          v                                             v
+--------------------------------------------------------------------------+
| store-owning process                                                     |
| Rust application OR foreground mount/run/MCP OR optional surrealfsd      |
|                                                                          |
| auth/policy as required -> surrealfs-core -> store adapter                |
|                                      -> embedded SurrealDB / SurrealKV    |
+--------------------------------------------------------------------------+
          |
          | logical events/exports, never required for local commit
          v
optional hosted/team control plane
```

The hosted plane is not required for local correctness. It may later receive logical events, signed
exports, summaries, or replicated repositories, but local commits never wait for a hosted service.

## Authoritative boundary and capture grades

SurrealFS can claim an atomic action-to-state transition only when its semantic kernel participates
in the authoritative publication transaction. Integration documentation and receipts expose one of
four grades rather than flattening unlike evidence into `captured`:

| Grade | Boundary | Permitted claim |
|---|---|---|
| `AUTHORITATIVE_ENFORCED` | SurrealFS owns publication and verifies the controlled process/workspace scope | Exact state transition with enforced action attribution |
| `AUTHORITATIVE_DECLARED` | An embedded Rust caller publishes atomically through the kernel but the caller is trusted for logical action identity | Exact state transition with declared attribution |
| `TRANSACTIONAL_BRIDGE` | An external runtime includes the SurrealFS receipt/root in the same authoritative transaction and passes conformance tests | Exact bridged transition within the tested vendor boundary |
| `OBSERVED` | Sidecar, FUSE observer, trace correlation, or receipt attached after a vendor checkpoint | Useful evidence or coarse restore point; never atomic causality |

An integration must downgrade when it cannot prove its grade. Running “alongside” a sandbox vendor
or attaching metadata to an already-created snapshot is `OBSERVED`, not exact. To preserve the core
promise with a vendor-owned filesystem, SurrealFS must become its workspace backend or obtain a
transactional bridge. Vendor integration permission is a distribution constraint, but open-source
or self-hosted runtimes may allow SurrealFS to implement the authoritative boundary directly.

### Publication proof is not input proof

The capture grade above describes authority over the resulting state transition. It does not claim
that SurrealFS knows every input the action consumed. Receipts separately carry one dependency basis:

| Basis | Meaning | Recovery use |
|---|---|---|
| `VERIFIED_OBJECTS` | A controlled SDK/tool API returned specific immutable object/root IDs | Exact delivered-object edges; not a claim about cognitive influence |
| `DECLARED_INPUTS` | The authenticated runtime declared its inputs, but SurrealFS did not mediate every read | Trusted according to integration policy and labelled as declared |
| `CONSERVATIVE_SCOPE` | The action may depend on an entire root, subtree, mount, or other named scope | Safe but deliberately broad affected-set calculation |
| `UNKNOWN` | No safe input bound is available, including uncontrolled external reads | No selective-independence claim; use a broader fork/replay boundary |

An `AUTHORITATIVE_ENFORCED` action may still have `CONSERVATIVE_SCOPE` or `UNKNOWN` dependencies:
SurrealFS can prove which process tree published root `B` from root `A` without proving which bytes
caused its behavior. FUSE/NFS callbacks, syscall traces, readahead, page-cache activity, `mmap`, and
inherited descriptors are observational evidence only and never upgrade a receipt to
`VERIFIED_OBJECTS`. Direct SDK/MCP/tool reads can return immutable IDs and therefore produce stronger
edges. An unmodified POSIX action defaults to the entire workspace base root plus explicit unknown
external inputs unless a stronger mediated source exists.

This keeps the design small: one publication grade, one dependency basis, optional object IDs/scope,
and an unknown-input flag. It does not require a universal read-tracing subsystem.

## Component responsibilities

### `surrealfs-core`

The embeddable core owns the domain state machine and, while open, the database handle:

- opens and migrates the store;
- acquires exclusive process ownership of the store directory;
- manages repository, branch, workspace, and session handles;
- mints and validates scoped workspace capabilities;
- maintains open file handles and advisory locks;
- applies domain policy, quotas, idempotency, and publication rules;
- exposes backup, export, integrity, and lifecycle operations;
- performs awaited shutdown and verifies the durability boundary.

No second process opens the same embedded database directory. Public callers receive domain handles,
not raw SurrealDB access.

### Runtime shells

A runtime shell embeds `surrealfs-core` when an operation needs a live process boundary:

- `surrealfs mount` hosts a FUSE or NFS mount in the foreground or under an ordinary supervisor;
- `surrealfs run` launches and binds a controlled process tree to a private workspace;
- `surrealfs serve mcp` hosts a session-scoped stdio server;
- optional `surrealfsd` provides versioned RPC, multiple mounts/clients, remote access, persistent
  reconciliation, centralized policy, or managed sandbox lifecycle.

These shells may combine in one process. A machine-wide daemon is not required by the storage or
commit design.

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

#### Normative content-object rules

The logical content format is public and versioned even though its physical SurrealDB layout is not.
Before Phase 1 writes persistent objects, the compatibility manifest and golden vectors freeze the
hash domain, manifest encoding, empty-file representation, path encoding, integer encoding, chunking
strategy, and extent rules. The current prototype assigns fixed 256 KiB chunks to root format v1;
Phase 0 must validate that choice against representative workloads before an external stable-format
claim. BLAKE3 hashes uncompressed content; compression is a storage detail and cannot change object
identity. Content-defined chunking is not adopted merely because it improves a synthetic prepend
case. Changing the strategy after persistent v1 data exists requires an explicit compatible strategy
or new format version and migration/interoperability tests.

Every ingest boundary—local stream, import, sync receive, restore, and repair—recomputes object and
manifest identities. A caller-supplied hash or a matching byte length is never sufficient. Branch
publication and remote-head acceptance fail closed when any newly reachable object is missing,
malformed, or corrupt.

The physical layout must split payload bytes from mutable metadata if SurrealDB serializes a
bookkeeping update as a rewrite of the combined large record. Phase 1 measures this on the pinned
engine; publication may not smuggle payload rewrites into its bounded transaction. Regardless of
table shape, payload objects are immutable and staging leases/GC bookkeeping cannot mutate their bytes. Staged objects remain
invisible until publication. GC traces all retained branches, snapshots, commits, recovery/export
holds, and in-flight staging leases; current heads alone are not a complete root set.

### Protocol layer

The service deployment exposes a versioned binary RPC over Unix-domain sockets locally and a
protected remote transport when required. It provides multiplexing, streaming, cancellation, flow
control, feature negotiation, and structured errors. Embedded callers use the same domain request
and response types without serialization or IPC.

Protocol messages use domain values. They do not expose SurrealDB values or physical record layouts.

Remote sync uses the same versioned object, root, commit, and receipt formats. It transfers immutable
objects before metadata and advances a branch only after complete reachability and hash verification.
Each remote has independent durable progress. A successful response echoes the durable applied
commit and root, which the sender checks against its own recorded state; response loss is resolved by
receipt/applied-head lookup. Head movement uses expected-head compare-and-swap and produces an
explicit conflict or divergence branch—never silent last-write-wins.

The protocol negotiates supported format versions and limits and streams missing objects with bounded
probes, backpressure, cancellation, and resumable request IDs. It does not reconstruct whole files in
memory merely to accept a manifest, flatten hardlinks into separate files, or treat a live path/rev
cursor as a historical snapshot. These requirements apply when Phase 10 remote sync is built; they do
not require a sync service in the initial embedded SDK.

### Mount adapters

FUSE and NFS translate protocol-specific requests into kernel commands. A mount process normally
embeds the core directly; it may instead connect to `surrealfsd` when multiple mounts or remote store
ownership justify the extra boundary. Mount adapters own request/reply translation and cache
invalidation behavior, not storage or filesystem truth.

The direct SDK/sandbox workspace is the reference write path. General shared mounts are later
adapters and cannot redefine publication on syscall, `close`, or `fsync`. A platform that cannot
enforce the workspace/process boundary ships with reduced, explicit guarantees or read-only support.

Mount implementations may use bounded per-handle write buffers for performance. `flush`, `fsync`, or
final `close` settles dirty data into the private workspace according to its durability policy but
does not create an action commit. Publication quiesces writers, flushes all dirty handles, verifies
the workspace, and fails rather than publishing a partial view. Process-memory buffers are an
optimization, not crash recovery; durable pre-publication journaling is added only if a measured
workflow requires it.

Cache correctness is part of the kernel contract: immutable content is cached by verified identity;
path and negative resolutions are scoped to an immutable root or workspace generation; no cache is
populated from uncommitted state; and every mutation path, including sync and recovery, uses common
invalidation hooks. Read-only mounts, capabilities, quotas, and protected-root checks are enforced in
the semantic kernel rather than only in an SDK, FUSE, or service wrapper.

Mount read observations may support debugging and conservative dependency scopes, but they are not a
semantic read set: kernel caching/readahead and memory mappings can over- or under-approximate actual
consumption, while host/network reads bypass the mount. The mount implementation therefore does not
add byte-level read tracing to the publication critical path.

### Rust SDK

The public Rust SDK provides ergonomic async APIs, type conversion, embedded open/create, optional
service connection, and streaming. Embedded and RPC modes run the same domain conformance fixtures.
Rust is the only planned client-language SDK.

## Write path

### Transactional workspace publication

```text
store owner opens workspace at expected branch head
  -> mint scoped capability and, when enforced capture is requested, process boundary
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

The store-owning process may process reads concurrently. Each writable tool gets a separate
workspace based on one commit. Workspace changes are isolated until publication. Publishes to one
branch use optimistic
concurrency against the branch head:

- every commit request includes `expected_head`;
- the transaction verifies the current head;
- a concurrent change produces a typed conflict;
- the caller may recompute against the new head or create a branch.

Different branches can commit concurrently when their records and shared uniqueness constraints do
not conflict. The kernel must never resolve a conflict by silently applying a stale plan.

The Linux proof forbids detached descendants and rejects or serializes nested writable workspaces.
Observational child spans may share their parent's workspace. General nested/concurrent write
semantics require a versioned design decision and cannot emerge accidentally from mount behavior.

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
| Workspace capabilities, open handles, transient locks | Store-owner memory plus hashed workspace metadata | Process-scoped authority; no bearer secret at rest |
| Workspace file/KV deltas | Private overlay/staging area | Hidden until explicit publish; discardable on abort |
| Logical exports | Portable archive outside database | Engine-independent recovery and interchange |
| Metrics | Telemetry pipeline, optionally summarized in DB | Avoid hot metrics writes contaminating commit path |

## Deployment modes

### Embedded Rust mode — required and first

One Rust application opens `surrealfs-core` and exclusively owns the selected store directory. This
is the initial SDK proof and has no daemon or RPC dependency. It proves atomic file/KV/action
publication, immutable roots, receipts, historical reads, forks, and recovery. The application is
trusted for declared action identity unless it uses the controlled launcher.

### Foreground runtime mode — required for mounts and unmodified tools

`surrealfs mount`, `surrealfs run`, NFS, or MCP embeds the same core and remains alive for the
session. The controlled Linux sandbox workspace is the reference for enforced process attribution.
The process may be supervised, but need not be installed as a machine-wide daemon.

### Service mode — optional and demand-gated

`surrealfsd` owns the store and exposes RPC when multiple clients/mounts, remote access, centralized
identity/policy, persistent background reconciliation, or managed sandbox lifecycle require it.
Service mode cannot change commit semantics and must pass the embedded conformance corpus.

### Container/CI mode — required

The foreground runtime or optional service and sandbox run in a container or VM with a persistent
data volume. Shutdown hooks improve operational behavior, but crash safety cannot depend on them.

### Remote repository service — later

Clients connect to `surrealfsd` or another service shell that owns the embedded store. Multi-tenant
scheduling, encryption,
replication, and failover are service concerns. A single SurrealKV directory is never opened by
multiple independent service processes.

### Browser mode — compatibility, not canonical

Browser clients, if ever added, connect to a remote service. An IndexedDB-only simulation may exist for demos,
but it must declare reduced guarantees and cannot define canonical semantics.

## Security and trust boundary

Embedded mode trusts the host application to declare its logical action identity; the kernel still
enforces transaction, root, idempotency, and history invariants. `AUTHORITATIVE_ENFORCED` mode treats
the agent/tool process as untrusted and requires a controlled launcher, opaque workspace capability,
private writable overlay, read-only committed lower state, inaccessible database directory, verified
process scope, and quiescence before publish. A trace ID, PID supplied by a client, or environment
variable never grants publication authority.

Every deployment must provide:

- an exclusive OS lock and private permissions for the store directory;
- parameterized database access through the adapter only;
- bounded paths, streams, graph traversals, mutation counts, and transaction sizes;
- explicit repository/tenant scoping and no cross-tenant content-deduplication oracle;
- capture minimization and redaction before logs, previews, events, or exports;
- encrypted transport for remote service mode and an explicit at-rest encryption claim;
- signed/checksummed migrations, logical exports, restore verification, and quarantine on integrity
  failure.

An embedding process that can mutate the store outside `surrealfs-core` invalidates the product
guarantee. Public SDK callers receive domain handles only.

## Lifecycle

### Open

1. Acquire exclusive store ownership.
2. Validate repository metadata and supported engine range.
3. Recover SurrealKV.
4. Apply or resume idempotent schema migrations.
5. Verify critical references and last durable receipt.
6. Start background tasks and accept requests.

The migration ledger is checksummed and monotonic. A store newer than the binary or in an unknown
migration state opens read-only or fails closed. Fresh creation and every supported upgrade path must
produce the same logical schema, constraints, indexes, and critical query plans; rebuilding a table
or record family may not silently lose a secondary index. Migration failure never leaves a writable
partially upgraded store.

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
- **Store-owner failure:** recovered database contains complete committed transactions only; open
  handles disappear and are reconciled.
- **Chunk-stage failure:** no branch state references incomplete bytes.
- **Metadata transaction failure:** no branch head or causal relation becomes partially visible.
- **Hosted service failure:** local operation continues; pending sync remains retryable.
- **Schema migration failure:** store remains on a declared migration state and refuses unsafe writes.
- **Integrity failure:** affected repository enters read-only/quarantine mode; repair never silently
  invents missing content.

## Cloudflare `dofs` adoption decisions

These decisions prevent both confirmation bias and unnecessary imitation. `dofs` is a preview
implementation optimized for a Durable Object authoritative live tree; SurrealFS is an immutable
root and action-publication system.

| Cloudflare technique | Decision | SurrealFS reason |
|---|---|---|
| Content-addressed chunks and manifests | Adopt | Required for deduplication, verification, and sparse object transfer |
| Stream/stage bytes before a short metadata transaction | Adopt | Keeps SurrealDB publication bounded and makes failure leave only invisible orphans |
| Verify received identities and complete reachability | Adopt and strengthen | `dofs` does not verify on receive at all: `stageBlob` documents that it "trusts the caller", and both receive paths pass wire bytes straight through. It then uses `ON CONFLICT(hash) DO UPDATE SET bytes = excluded.bytes`, so a bad pair overwrites an already-correct payload, where the local write path uses the safe `DO NOTHING`. SurrealFS verifies every received identity before it is reachable |
| Per-remote progress plus echoed applied state | Adopt | Makes distributed convergence assumptions observable and testable |
| Transaction-aware/root-scoped caches | Adopt | Prevents rolled-back or cross-root state from surviving in cache |
| Data-layer mount/policy enforcement | Adopt | Wrapper-only checks are bypassable by sync or alternate surfaces |
| Fresh-versus-upgraded migration equivalence and query-plan checks | Adopt | Prevents schema/index drift and hidden performance regressions |
| Per-handle FUSE buffering | Adopt only as an optimization | It reduces write amplification but cannot define publication or recovery |
| Cloudflare's fixed 512 KiB chunks | Do not copy; benchmark current 256 KiB v1 | Chunk size is workload- and format-specific; insertions near the head remain a known weakness |
| Separate mutable metadata and payload | Conditional requirement | Split when the pinned engine would rewrite payload bytes during bookkeeping/publication |
| Path/revision live-state sync | Reject | Immutable commits and roots already provide stronger history and resume semantics |
| Silent last-write-wins | Reject | Expected-head conflicts and divergence preservation are core product guarantees |
| Whole-file assembly during sync | Reject | Manifests and chunks can be verified and installed incrementally |
| Flattening hardlinks on the wire | Reject | `dofs` models hardlinks correctly in its own schema and loses that identity only in transfer, emitting one entry per name; SurrealFS carries inode identity through publication and sync |
| Lockstep, unversioned wire deployment | Reject | Independent SDK/runtime adoption requires negotiated formats and fixtures |
| AgentFS or `dofs` runtime dependency | Reject | Neither matches the SurrealDB-backed semantic kernel; reuse specifications and tests instead |

## External effects and combined recovery

SurrealFS can restore filesystem/KV state exactly and can atomically commit that state with an
external-effect intention. It cannot atomically reverse GitHub operations, deployments, emails,
payments, cloud mutations, or arbitrary databases. The recovery contract therefore separates local
truth from external truth.

For a controlled integration, the authoritative sequence is:

```text
prepare effect intent + operation digest + idempotency key
  -> commit intent with local action receipt
  -> dispatch through the approved adapter
  -> record attempt and provider evidence
  -> CONFIRMED | FAILED | UNKNOWN
  -> reconcile UNKNOWN by provider lookup/marker/idempotency record
  -> optionally prepare and dispatch a separate compensating effect
```

The canonical effect records are `Effect`, `Attempt`, `Evidence`, `Resolution`, and optional linked
`Compensation`. A response lost after provider application becomes `UNKNOWN`; SurrealFS never blindly
retries it. Compensation is a new external effect with its own success, failure, or unknown outcome,
not deletion of the original fact.

One recovery workflow combines all three dimensions:

1. select the harmful logical action and verify its before/after filesystem/KV roots;
2. traverse only dependencies justified by their recorded basis; expand conservative scopes and
   refuse selective-independence claims across unknown inputs;
3. fork or restore the exact safe local root without rewriting the original history;
4. enumerate external effects after that root and reconcile every `UNKNOWN` outcome;
5. compensate only where the provider and policy permit it;
6. emit an explicit result such as `LOCAL_EXACT_EXTERNAL_UNCHANGED`,
   `LOCAL_EXACT_EXTERNAL_COMPENSATED`, or `LOCAL_EXACT_EXTERNAL_DIVERGED`.

The UI and demo must show irreversibility and divergence rather than hiding them in disclaimers.
Exact state restoration controls the starting state; it does not make model, network, time, random,
or scheduler behavior deterministic.

Local state recovery is therefore always available in a conservative form: when evidence is weak,
treat the whole base root/scope as depended upon and fork before the action. Fine-grained
transplant or selective undo is offered only when verified or policy-accepted declared edges prove
that later work is independent. POSIX read observations may add debugging context but never shrink
the conservative affected set by themselves.

## Required conformance evidence

Every deployment shape must pass the same canonical command/root corpus where its platform semantics
permit. Before a production claim, the evidence includes:

- reference-model and generated filesystem/KV/action sequences;
- concurrent expected-head and idempotency campaigns;
- deterministic termination before/after commit, response loss, reopen, compaction, and lock release;
- embedded versus optional-service equivalence;
- SDK/FUSE/NFS/MCP/sandbox root equivalence;
- receipts that keep publication grade independent from dependency basis, including direct-API,
  declared, whole-root POSIX, and unknown-external-input fixtures;
- forged capability, path escape, detached child, raw-database, and cross-scope negative tests;
- logical export/import root verification and stopped-store recovery-copy drills;
- deterministic fake-provider tests for apply-plus-response-loss, reconciliation, compensation, and
  permanent divergence;
- representative p50/p95/p99, memory, disk amplification, startup, and graph-query measurements.

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
