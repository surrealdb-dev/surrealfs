# Canonical data model

## Design goals

The data model must support fast current-state operations, durable application history, graph
traversal, deterministic retry, repository isolation, branch ancestry, and backend-independent
logical export.

It deliberately does not use engine-level temporal versioning as the only source of history. Explicit
records keep branch and retention semantics under SurrealFS control and make exports intelligible
without reproducing SurrealKV internals.

## Identity rules

### Repository-scoped identity

Every system-owned record contains `repository: record<repository>` even when its ID also embeds the
repository identifier. This makes permission checks, validation, export, and corruption detection
explicit.

### Deterministic and random IDs

- Repository, agent, run, span, branch, and snapshot identities use caller-generated UUIDv7/ULID-like
  sortable random IDs.
- Commits use a deterministic digest of versioned canonical commit content plus repository identity.
- Chunks use BLAKE3 of uncompressed bytes.
- Artifacts use a digest of their versioned manifest and semantic metadata.
- Retriable relation records use deterministic IDs derived from relation type and endpoint IDs.
- Current-state record IDs derive from logical keys such as `(repository, branch, inode)`.
- History record IDs derive from logical key plus commit sequence or commit ID.

Canonical encoding must be versioned. Hash inputs never depend on JSON object ordering, local time
formatting, or database-generated IDs.

## Top-level records

### `repository`

| Field | Meaning |
|---|---|
| `id` | Stable repository record ID |
| `created_at` | Informational timestamp |
| `format_version` | Logical data-format version |
| `schema_version` | Applied physical schema migration |
| `default_branch` | Branch record |
| `state_hash_version` | Algorithm/version used for state roots |
| `retention_policy` | Named snapshots, history, chunks, and event retention |
| `encryption_profile` | Logical reference to key-management policy |
| `status` | active, read-only, migrating, quarantined |

### `agent`

Represents a stable configured actor rather than one invocation.

Important fields: provider-neutral name, configuration digest, framework/integration identifiers,
declared capabilities, creator, labels, and lifecycle timestamps. Secrets are referenced, not stored
in plaintext configuration.

### `run`

A top-level attempt from a defined starting point.

| Field | Meaning |
|---|---|
| `agent` | Agent record |
| `starting_commit` | Exact initial state |
| `branch` | Branch mutated by the run |
| `status` | pending, running, success, error, cancelled, interrupted |
| `goal_digest` | Digest of potentially sensitive goal text |
| `goal_ref` | Optional encrypted/redacted payload reference |
| `environment_digest` | Captured environment manifest |
| `started_at/completed_at` | Informational time |
| `root_span` | Root execution span |
| `result_artifacts` | Selected outputs |

### `span` and `tool_call`

`span` is the general nesting and causality primitive. `tool_call` stores tool-specific structured
fields and references its span. Separating them permits process, model, human approval, internal
workflow, and tool spans to share one hierarchy.

Span fields include run, parent span, type, name, status, input/output digests, start/end timestamps,
error classification, retry-of reference, process identity, and capture level.

Tool-call fields include tool schema/name/version, parameter digest/payload reference, result digest/
payload reference, external side-effect declaration, and framework invocation ID.

### `branch`

| Field | Meaning |
|---|---|
| `name` | Unique within repository |
| `head` | Current commit |
| `base_commit` | Commit from which the branch was created |
| `parent_branch` | Optional ancestry hint |
| `head_sequence` | Branch-local monotonic sequence |
| `generation` | Incremented on head update for diagnostics |
| `created_by` | Span/user/system cause |
| `status` | active, read-only, archived |

The branch record is the concurrency fence. Updating it requires the expected previous head.

### `snapshot`

A unique repository-scoped name, immutable target commit, creator cause, creation time, labels,
retention pin, and optional signature. Renaming a snapshot creates or updates the name reference; it
does not alter the target commit.

### `commit`

| Field | Meaning |
|---|---|
| `id` | Deterministic commit record ID |
| `branch` | Branch advanced by the commit |
| `sequence` | Branch-local sequence |
| `parents` | One parent normally, two or more for merge commits |
| `cause` | Span, import, merge, admin, or system cause |
| `request_id` | Idempotency identity |
| `mutation_count` | Ordered mutation total |
| `mutation_root` | Digest of canonical mutations |
| `state_root` | Digest of logical state after commit |
| `capture_level` | semantic, strict, imported, partial |
| `created_at` | Informational timestamp |
| `durability_mode` | Mode under which it was acknowledged |
| `schema_version` | Domain encoding version |

Commit records are immutable after creation. Completion data that cannot be known before commit is
stored in separate result/event records rather than mutating the commit's identity fields.

### `mutation`

One ordered operation in a commit. Fields include commit, ordinal, kind, logical target, previous
generation/content digest, next generation/content digest, and a typed payload. The mutation list is
canonical evidence and supports efficient commit diffs.

Mutation kinds include:

- inode create/update/delete;
- dentry create/replace/delete;
- extent replace/truncate/punch-hole;
- symlink target update;
- xattr set/delete;
- KV set/delete;
- branch metadata update;
- artifact declare/attach;
- merge resolution.

## Filesystem records

### `inode`

Materialized current inode at one branch head. Its logical ID is `(repository, branch, inode_id)`.

Fields:

- stable numeric or 128-bit `inode_id`;
- kind: regular, directory, symlink, device where supported;
- mode, owner, group;
- logical size;
- link count;
- atime, mtime, ctime with explicit update policy;
- device metadata where supported;
- generation;
- last commit;
- content/extent root digest;
- xattr root/reference;
- deleted/open-unlinked state where applicable.

### `inode_version`

Immutable version containing the same logical inode fields plus commit, sequence, previous version,
and branch. It exists even when current state later deletes the inode; deletion is represented by a
tombstone version.

### `dentry`

Materialized mapping `(repository, branch, parent_inode, normalized_name_bytes) -> child_inode`.

The original byte name is retained. Name normalization and case sensitivity are repository-format
properties and cannot change in place.

### `dentry_version`

Immutable mapping or tombstone at a commit. It records parent, name, child, kind hint, generation,
previous version, and last commit.

### `file_extent`

Materialized non-overlapping file extents ordered by file offset. Each extent is either a hole or
references `(chunk, chunk_offset, length)`. Extent IDs include repository, branch, inode, and logical
offset/order. Extent replacement is versioned through `file_extent_version` or a versioned immutable
extent-map manifest, selected after benchmark comparison.

### `symlink_data`

Small targets may be fields on inode records; a separate record is acceptable when uniform content
handling is simpler. Symlink reads never resolve the target in storage; path traversal does.

### `xattr`

Current xattrs key by repository, branch, inode, and attribute name. Immutable versions capture set
and deletion. Security-sensitive namespaces require explicit platform and policy handling.

## KV records

### `kv_entry`

Materialized `(repository, branch, namespace, key_bytes)` value with generation, last commit, optional
expiry, content type, and value digest.

### `kv_version`

Immutable value or tombstone with commit, sequence, previous version, and expiry semantics. Expiration
is a committed system mutation when it changes visible state; wall-clock filtering alone must not
make historical state ambiguous.

## Content and artifact records

### `chunk`

| Field | Meaning |
|---|---|
| `id` | BLAKE3 digest of uncompressed bytes |
| `bytes` | Binary payload, potentially placed in SurrealKV VLog |
| `length` | Uncompressed length |
| `codec` | none or approved compression codec |
| `stored_length` | Physical payload length |
| `created_at` | Operational timestamp |
| `verification_state` | staged, verified, quarantined |

Chunk records are immutable. Reference count is not a source-of-truth field because retries and crash
recovery can make eager counters fragile. Reachability is derived or maintained as a repairable
index, with GC protecting recent staged chunks.

### `artifact`

An artifact is a semantic object, not simply a file path. It contains kind, manifest digest, content
root, producer commit/span, labels, media type, logical size, and optional path associations.

### `artifact_manifest`

An immutable ordered description of content chunks, directory trees, multi-file bundles, or external
references. The manifest encoding is versioned and hashed.

## Evaluation and policy records

### `evaluation`

References the subject run/commit/artifact/branch, evaluator identity and version, input evidence,
score/assertions, status, explanatory payload, baseline/comparison target, and creation span.

### `policy`

Immutable or versioned policy definition with scope, version, source digest, and activation interval.

### `policy_decision`

Records policy, subject action/span, evidence digest, decision, constraints, approval requirement,
human/system decider, and enforcement result. Policy records do not contain secret source material by
default.

## Relation tables

SurrealDB relation records are canonical graph edges. Required relations include:

| Relation | From | To | Meaning |
|---|---|---|---|
| `invoked` | run/span | tool_call | Invocation membership/order |
| `caused` | span/tool_call | commit | Direct causal attribution |
| `produced` | commit/span | artifact | Artifact creation |
| `observed` | span/tool_call | commit/state/artifact | Captured observation |
| `read` | span/tool_call | artifact/state reference | Declared or strict read dependency |
| `wrote` | span/tool_call | state reference | Semantic write summary |
| `derived_from` | artifact | artifact | Derivation provenance |
| `forked_from` | branch/run | commit/run | Fork ancestry |
| `evaluated` | evaluation | run/commit/artifact | Evaluation subject |
| `governed` | policy_decision | span/tool_call | Governed action |
| `parent_of` | span | span | Explicit hierarchy where useful |
| `merged_from` | commit | commit/branch | Merge ancestry and source |

High-volume reads and writes may use summarized set records rather than one edge per syscall. Capture
level is explicit so queries can distinguish semantic declarations from strict observation.

## Idempotency records

`request_receipt` keys by repository and request ID. It stores request digest, resulting commit,
response digest, completion status, and expiry/pinning policy.

Rules:

- same ID and same request digest returns the original receipt;
- same ID and different digest returns an idempotency violation;
- receipt creation is in the commit transaction;
- receipts live long enough to cover all supported retry windows;
- pinned import/sync operations may retain receipts indefinitely.

## Materialized state versus history

Every successful commit writes:

1. immutable versions and tombstones;
2. materialized current records for the new branch head;
3. commit and mutation evidence;
4. relations and request receipt;
5. branch-head update.

Materialized state is a cache with transactional guarantees. A verification/rebuild operation can
derive it from branch ancestry and history. Production reads use it for latency.

## Branching model

A branch initially points to `base_commit` and may have no branch-local materialized changes. Reads
resolve branch-local current records first, then the base view. Three implementation strategies must
be benchmarked:

1. eagerly materialize all current records at fork;
2. layered lookup with base fallback;
3. persistent immutable tree roots plus branch-local overlays.

The first production choice should optimize for correctness and representative branch sizes. Layered
lookup is the expected starting point, with background flattening once branch depth or read latency
crosses a measured threshold.

## State roots

State roots provide integrity and comparison, not primary lookup. The algorithm must define:

- canonical ordering of filesystem and KV logical keys;
- canonical encoding of metadata and content references;
- inclusion/exclusion of volatile timestamps;
- tombstone treatment;
- branch/base resolution;
- hash algorithm and version;
- whether filesystem and KV roots are separate children of a repository root.

Incremental Merkle structures may be introduced after the vertical slice. Initial correctness may
compute roots expensively for fixtures and asynchronously verify production commits.

## Index plan

At minimum index:

- repository on every system table;
- unique branch name within repository;
- branch head and base commit;
- commit by branch/sequence and parent;
- current dentry by branch/parent/name;
- current inode by branch/inode ID;
- extent by branch/inode/offset;
- KV current key by branch/namespace/key;
- history by logical key/sequence and commit;
- span by run/parent/status/start;
- tool call by run/name/status;
- artifact by content root/producer/kind;
- evaluation by subject/evaluator/status;
- request receipt by repository/request ID;
- relation endpoints required by product traversals.

Every index must justify a query and have a cardinality/maintenance benchmark. Avoid indexing large
payload fields.

## Retention and garbage collection

Retention acts on reachability and policy:

- active branch heads and named snapshots are roots;
- pinned runs/evaluations/artifacts extend reachability;
- commits required by retained ancestry remain;
- chunks referenced by retained extents/manifests remain;
- recent staged chunks are protected for retry;
- deletion produces auditable GC plans before physical reclamation where required;
- legal hold overrides ordinary retention;
- logical exports can pin a consistent retention boundary.

GC must be resumable and idempotent. Reference counts may accelerate it but cannot be the only proof
of reachability.

## Schema evolution

Every migration has:

- monotonically ordered ID;
- precondition and supported source versions;
- idempotent start/complete markers;
- forward data transformation;
- validation query;
- rollback or restore strategy;
- expected lock, disk, and time requirements;
- compatibility window for rolling daemon/SDK deployment.

Large data rewrites use shadow fields/tables and resumable backfills. A migration must not make old
commits unverifiable without retaining the prior canonical encoding version.

