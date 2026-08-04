# SurrealDB + SurrealKV Candidate Operating Contract

## Decision

SurrealFS will first prove SurrealDB embedded in `surrealfsd`, with SurrealKV underneath, as its
preferred canonical-store candidate. It becomes the only persistent engine in the first production
architecture only if it passes the Phase 0 parity spike and all gates below. This document specifies
the candidate's operating contract; it is not evidence that the contract has passed.

```text
SurrealFS domain commands
          |
          v
typed repository layer + reviewed SurrealQL
          |
          v
embedded SurrealDB query/transaction engine
          |
          v
SurrealKV files owned exclusively by surrealfsd
```

Within this candidate, this is one database, not two databases. SurrealDB provides records, relations, indexes,
transactions, and queries. SurrealKV is the key-value persistence layer under that database. A
SurrealFS commit is a domain transaction in SurrealDB; there is no application-level replication
between a graph store and a key-value store.

The official documentation describes SurrealDB as a multi-model engine and SurrealKV as a beta
storage option aimed at embedded and local-first workloads. Those characteristics match the
intended deployment, but the beta label creates a real production-readiness obligation rather
than a marketing advantage:

- [SurrealDB storage-engine overview](https://surrealdb.com/docs/build/embedding/storage-engines)
- [Rust embedding API](https://surrealdb.com/docs/reference/rust/embedding)
- [SurrealDB license FAQ](https://surrealdb.com/license)

## Why this architecture is worth trying

SurrealFS needs two kinds of access to the same facts:

1. Point and range access for paths, inodes, branch heads, extents, chunks, leases, and receipts.
2. Relational and graph access for runs, tool calls, commits, artifacts, forks, policies, and
   evaluations.

SurrealDB over SurrealKV allows both to participate in the same transaction. A tool call can write
a file, update agent KV, create a commit, move a branch head, link an artifact, and close its span
without a projection queue or cross-database compensation protocol. That substantially reduces
the surface for false provenance.

It also makes a local-first product plausible: a single daemon can own a single directory, keep
working without a network service, and expose higher-level APIs to any language.

## Why not expose SurrealKV directly

SurrealKV-only mode is deliberately excluded from the production API in the initial design.
Supporting it would require SurrealFS to implement and maintain:

- its own secondary indexes;
- graph adjacency lists and traversal logic;
- schema validation and migrations;
- query planning or a second query interface;
- subscriptions/change notification;
- transaction encoding for every relation and materialized view;
- two independent compatibility suites if combined mode also remains supported.

That work does not improve the user-visible moat. It spends engineering capacity rebuilding parts
of SurrealDB while weakening the guarantee that the graph and filesystem are one truth.

An internal storage trait is still useful for unit tests and for preserving architectural
discipline, but it represents the semantic operations SurrealFS needs, not a promise that arbitrary
key-value engines are interchangeable:

```rust
#[async_trait]
pub trait RepositoryStore {
    async fn commit(&self, command: CommitCommand) -> Result<CommitReceipt>;
    async fn read_node(&self, at: CommitId, path: &RepoPath) -> Result<Node>;
    async fn diff(&self, from: CommitId, to: CommitId) -> Result<Diff>;
    async fn explain(&self, target: ExplainTarget) -> Result<ExplanationGraph>;
}
```

An in-memory implementation may back tests. A future engine is accepted only if it implements the
same invariants and passes the same conformance and crash suites.

## Mandatory Phase 0 parity spike

The engine decision uses one deliberately small domain scenario implemented twice:

```text
open at expected head -> stage file/KV/artifact delta -> publish immutable root + span + receipt
-> reopen -> historical read -> first-parent log -> fork -> diff -> explain
```

Implement it on:

1. embedded SurrealDB over SurrealKV with `sync=every` for durable acknowledgement;
2. SQLite/AgentFS or a clean SQLite adapter using WAL and `synchronous=FULL` for comparable durable
   acknowledgement.

Both implementations use identical canonical IDs, mutations, roots, fixtures, fault outcomes,
logical export, and user-level queries. Correctness is pass/fail. After correctness, compare adapter
and migration complexity, lines/components owned, p50/p99 latency, CPU/memory, disk/write
amplification, reopen/lock release, export/restore, ancestry/causal-query plans, and implementation
time. Publish raw configuration and results.

The spike also compares extending AgentFS with semantic commits against establishing a separate
SurrealFS stack. It does not create a promise to maintain two production backends. Select
SurrealDB/SurrealKV only when it passes every invariant and materially reduces domain/index/query
complexity or accelerates the validated recovery workflow.

## Dependency boundary

SurrealFS application crates must use the supported embedded SurrealDB SDK and reviewed SurrealQL.
They must not import SurrealDB's internal KVS crates or SurrealKV implementation types directly.

The current private tree makes this boundary clearer than the earlier baseline. It now separates a
typed `engine-api`, `engine-local`, `datastore`, `kvs`, `kvs-any`, and backend adapter packages.
Those packages are useful evidence of a cleaner internal architecture, but explicitly carry no
SemVer stability guarantee. Depending on them would turn every database upgrade into a SurrealFS
refactor. The only crate allowed to know the concrete embedded endpoint is the
`surrealfs-store-surreal` adapter, and it still uses the public `surrealdb` crate.

The public SDK contains a hidden `unstable_from_datastore` constructor. SurrealFS must not use it:
its source comment explicitly says it exposes internal types and is not stable. If the product needs
a lifecycle capability absent from the SDK, that capability should be added to the supported public
surface rather than reached through this escape hatch.

Dependency direction:

```text
surrealfs-domain       no database dependency
surrealfs-protocol     no database dependency
surrealfs-store        domain storage trait only
surrealfs-store-surreal -> pinned surrealdb SDK + SurrealQL migrations
surrealfsd             composes store, services, auth, mount adapters
SDKs                   RPC protocol only
```

No SDK may open the database directory. No FUSE callback may issue ad hoc SurrealQL. No product
component may mutate a SurrealFS table except through the semantic command service.

## Version and feature policy

The current implementation baseline inspected for this plan is `v3.3.0-nightly`, source commit
`e68539867728aa6412a75c7669b0b33c30c00feb`, with SurrealKV `0.21.3`. Git describes the checkout as
`v3.0.0-beta.1-742-ge68539867`. This is a development baseline, not an automatic production
endorsement. It supersedes the earlier design-review baseline `c94d6584e`.

The full revalidation, including the 26-commit architectural diff and exact executed tests, is in
the [current private-source audit](15-current-surrealdb-audit.md).

The repository must pin all of the following in its lockfile and release manifest:

- exact SurrealDB crate version or Git revision;
- enabled cargo features, including `kv-surrealkv` and excluding unused engines;
- Rust toolchain version;
- schema version and logical export version;
- SurrealFS protocol version;
- database configuration fingerprint.

No broad semver range is permitted for the embedded engine. Renovation is a tested release event,
not a background dependency bump.

The build should expose:

```text
surrealfs version --verbose
  surrealfs:       0.x.y
  protocol:        N
  schema:          N
  export-format:   N
  surrealdb:       exact revision
  surrealkv:       exact revision
  rustc:           exact version
  engine-config:   hash
```

## Initial SurrealKV configuration

The values below are starting hypotheses. They must be benchmarked on agent workloads before a
release is called durable or production-ready.

| Setting | Initial position | Reason |
|---|---|---|
| Sync mode | Sync every durable commit | Acknowledged state must survive an ordinary process crash |
| Group commit | Enabled with a small bounded window | Amortizes fsync while preserving a clear acknowledgment point |
| Value log | Enabled | Separates large values such as metadata payloads and chunk-adjacent data |
| Value-log threshold | 4 KiB starting point | Matches the inspected engine default; benchmark 1, 4, 16, and 64 KiB |
| Block size | 64 KiB starting point | Matches the inspected default; validate directory scans and graph reads |
| Versioning | Off for correctness | Branch/snapshot history lives in SurrealFS commit records |
| Versioned index | Off unless temporal diagnostics are enabled | Avoid duplicate history and unbounded retention cost |
| Cache capacity | Explicit deployment budget | Never accept a machine-relative default unnoticed in constrained agents |
| Database directory | Absolute, private, single-owner path | Avoid cwd-dependent placement and competing openers |

The inspected configuration defaults enable the value log, use a 4 KiB value threshold and 64 KiB
blocks, keep temporal versioning off, and sync every commit. The public environment-variable
reference documents these tuning controls and must be checked against the exact pinned release:
[SurrealKV configuration reference](https://surrealdb.com/docs/reference/cli/surrealdb-cli/environment-variables).

### Durability profiles

SurrealFS presents named domain profiles instead of leaking storage-engine switches:

| Profile | Acknowledgment rule | Intended use |
|---|---|---|
| `durable` | Data and commit metadata pass the engine's grouped WAL fsync before success | Default repositories and any externally visible result |
| `ephemeral` | No survival promise; memory-backed tests only | Unit tests, previews, scratch runs |

There is no separate user-facing `grouped` profile: the current `sync=every` implementation already
groups concurrent commit waiters behind a durable `flush_wal(true)` acknowledgment. There is also no
user-facing `unsafe-fast` persistent mode in v1. A mode that acknowledges before durability would
infect recovery semantics, receipts, and documentation for marginal benefit.

## Temporal versioning policy

SurrealKV temporal versioning is not the branching mechanism. SurrealFS stores immutable
application commits and explicit parent links because it needs semantic information that engine
versions do not contain: author span, mutation set, branch, message, state root, policy decision,
and artifact causality.

Engine versioning may later be enabled for short operational windows to support database-level
diagnostics or changefeeds. If enabled:

- retention is finite and documented;
- it cannot be required for a SurrealFS checkout, diff, fork, or export;
- its storage overhead is measured separately;
- a compaction or retention change cannot delete application history;
- tests prove identical SurrealFS behavior with the feature disabled.

## Ownership and process model

Only `surrealfsd` opens the embedded database directory. This is the single-semantic-writer rule,
not necessarily a single-client rule: many SDK clients and mount requests may execute
concurrently, but the daemon serializes or conflicts changes at repository branch heads.

On startup the daemon must:

1. acquire an exclusive OS-level lock in the repository service directory;
2. validate directory ownership and permissions;
3. read the release manifest without mutating state;
4. compare engine, schema, and export compatibility;
5. recover or reject interrupted migrations;
6. open the database;
7. run read-only invariant probes;
8. expose its socket only after health checks pass.

On shutdown it stops accepting writes, drains bounded in-flight transactions, flushes the engine,
writes no false "clean" marker until flush succeeds, closes the database, and releases the lock.
SIGKILL safety still comes from the storage protocol, not the clean marker.

The current public Rust SDK does not expose an obvious awaited shutdown/close operation. Dropping
all SDK handles closes the route channel, after which the internal local-engine task invokes
datastore shutdown. The current SurrealKV shutdown implementation logs WAL-flush or close errors and
returns success. Phase 0 must therefore test SDK-drop, shutdown-under-load, reopen, and lock release;
it must not claim a verified clean shutdown until the supported public lifecycle has an observable,
error-reporting boundary. This gap does not weaken the rule that acknowledged commits must survive
SIGKILL without a graceful close.

## Queries, transactions, and subscriptions

All mutations use the public SDK's explicit client transaction API. `Surreal::begin()` returns a
transaction through which the adapter issues its checked operations before `commit()` or
`cancel()`. Query text is stored in source files, parameterized, assigned a stable operation name,
and exercised by integration tests. Dynamic string interpolation is forbidden. SurrealFS does not
construct transactions through internal KVS traits.

The engine distinguishes retryable transaction conflicts from an unknown commit outcome internally.
The domain protocol cannot treat all transport failures as safe retries: after an ambiguous result it
queries the deterministic request receipt, returns the stored outcome if present, retries only when
absence is proven, or exposes an explicit reconciliation state.

Live queries may power local dashboards, but they are not a durable event bus. Official guidance
states that live notifications describe committed changes but do not provide a universal total
order across writers; catch-up consumers should use durable domain sequence numbers and commit
queries. See [live query behavior](https://surrealdb.com/docs/learn/querying/real-time/live-queries).

The daemon subscription API therefore works as follows:

1. client supplies `after_sequence`;
2. server reads durable events after that cursor;
3. server switches to live notification;
4. server de-duplicates by event ID;
5. client persists its acknowledged cursor;
6. reconnect repeats from the cursor.

## Backup and restore

SurrealFS supports two distinct backup products.

### Physical recovery copy

A physical recovery copy is engine-specific and intended for fast restoration to the same compatible
engine line. The current embedded SDK's advertised `Backup` capability is logical SurrealQL
export/import, not a proven SurrealKV live-snapshot API. Until a supported physical snapshot exists,
SurrealFS creates a physical copy only after the datastore is stopped, or through a separately
validated quiescent procedure. Copying live files with a generic filesystem copy is not a backup
protocol.

The physical-copy manifest includes checksums, engine revision, configuration fingerprint, schema
version, created-at time, and last durable domain sequence. Restore occurs into a new empty target,
is verified before serving, and never overwrites the source automatically.

### Logical export

The logical export is the portability and disaster-recovery contract. It contains a versioned,
engine-independent stream of:

- repository and branch metadata;
- commits, parent edges, ordered mutations, and state roots;
- inode/dentry/extent state necessary to materialize every retained commit;
- KV versions;
- execution records and graph relations;
- artifacts and chunk manifests;
- content-addressed chunks, optionally in separate pack files;
- policy, evaluation, and schema metadata;
- a terminal index and cryptographic checksums.

Export reads from one explicit commit boundary. Import is idempotent, validates every record and
chunk hash, recomputes state roots, and does not advance a visible branch until validation passes.

The public SDK's logical export is useful evidence and targeted export/import tests pass on the
current revision. SurrealFS nevertheless owns the product format because it must include external
content packs, canonical IDs/order, state roots, domain versioning, and a terminal verification
index independent of SurrealQL's physical representation.

Release policy requires a successful logical export and restore test before every engine upgrade.

## Upgrade protocol

An upgrade is a controlled state transition:

1. Publish a compatibility entry covering old engine, new engine, schema, and export versions.
2. Pass the full conformance, crash, migration, export/import, and workload benchmark suites.
3. Create and verify a logical export using the old binary.
4. Create a supported physical snapshot where available; otherwise create a verified stopped-store
   recovery copy.
5. Stop writers and record the last durable domain sequence.
6. Open a cloned database with the new binary and run migration plus invariant scans.
7. Compare branch heads, state roots, record counts, graph-edge counts, and sampled materialization.
8. Exercise read-only shadow traffic against the clone.
9. Upgrade a canary repository and monitor reopen, compaction, memory, latency, and disk growth.
10. Roll out gradually. Preserve the old binary and logical export until the rollback window ends.

Downgrade-in-place is never assumed. Rollback means serving a verified pre-upgrade copy or
restoring a logical export with a compatible binary.

## Evidence collected for the decision

The following checks were run against current private source revision `e68539867` during this
revalidation. All used an isolated target directory under `/private/tmp`:

```text
cargo test -p surrealdb-kvs-any --features kv-surrealkv --test kvs
  result: 73 passed, 0 failed, 8 ignored

cargo test -p surrealdb-kvs-surrealkv
  result: 3 passed, 0 failed

targeted public SDK api tests with kv-surrealkv,parse
  client-side transactions: passed
  session transaction isolation: passed
  live select: passed
  logical export/import: passed
  versioned select: passed
  versioned export/import data types: passed
```

The ignored shared-adapter cases included low-level versioned behavior because that harness opens
the default non-versioned configuration, plus backend/harness-specific cases not covered by the run.
The separate versioned public-SDK targets listed above passed. This evidence proves that selected
surfaces passed on one development machine. It does **not** prove SurrealFS crash consistency,
workload fitness, multi-platform behavior, long-run compaction stability, or release readiness.

SurrealKV `0.21.3` is especially relevant: its update commit records an fsync-after-compaction fix
for a SIGKILL/zero-byte-SSTable startup failure and a memtable-rotation fix. Those fixes improve the
baseline but also establish concrete regression cases for the SurrealFS crash campaign.

## Production validation gates

SurrealDB + SurrealKV remains a conditional choice until all of these gates pass:

- mandatory parity spike and build-vs-extend report completed without durability mismatch;
- immutable-root, workspace-isolation, attribution, ancestry, and idempotency conformance passed;
- 72-hour write/read/reopen soak with no invariant violation;
- deterministic process-kill fault injection at every commit boundary;
- power-loss or storage fault simulation where the platform permits it;
- repeated backup/restore and logical export/import drills;
- filesystem and graph workload benchmarks on macOS and Linux;
- bounded memory and disk amplification at target repository size;
- compaction does not cause unacceptable tail-latency stalls;
- exact release supports required transaction, backup, and embedded behaviors;
- public SDK lifecycle releases the store reliably and any required awaited shutdown can report
  failure without internal API use;
- schema migrations are forward-safe and rollback is demonstrated;
- independent license review approves the intended distribution and hosted model.

If these gates fail, the fallback is not an exposed SurrealKV mode. The fallback is to preserve the
SurrealFS domain, RPC, export, and conformance contracts and replace the storage adapter. This is
why the semantic boundary matters.

## Licensing and commercial constraints

SurrealKV is Apache-2.0 in the inspected source. SurrealDB describes its database source under the
Business Source License and states that use in applications and managed-database offerings can
have different implications. SurrealFS must obtain legal review for the exact source revision,
features, linking/distribution method, customer deployment model, and whether the product could be
construed as a managed database service. Website summaries are not legal advice.

License approval is a Phase 0 exit gate. If the intended commercial model needs a SurrealDB
agreement, that cost and dependency must enter the business case before implementation grows.

## Decision triggers

Re-open the storage decision if any of the following occurs:

- SurrealKV remains beta when SurrealFS needs a stable production SLA;
- crash or compaction testing reveals integrity or tail-latency failures;
- embedded backup/restore cannot meet the operational contract;
- the supported SDK cannot provide an acceptable shutdown/reopen lifecycle;
- SurrealDB licensing prevents the intended distribution or economics;
- the SQLite/AgentFS baseline provides equivalent semantics with materially lower lifecycle,
  implementation, or reliability risk;
- query overhead makes filesystem metadata operations miss target latency by more than the allowed
  budget after profiling and schema optimization;
- engine API churn consumes more than ten percent of quarterly engineering capacity;
- a second process must safely write the same local database directory;
- the product moves from local/single-node operation to horizontally distributed writes.

The criterion is not whether another database wins a generic benchmark. It is whether this stack
can uphold SurrealFS's domain contract at an acceptable reliability and engineering cost while
materially simplifying the validated product workflow.
