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
- Persistent state-node/root IDs derive from repository, root-format version, node kind, and
  canonical content digest.
- Optional materialized-head projection IDs derive from `(repository, branch, logical key)` and are
  excluded from canonical identity.
- Mutation/evidence record IDs derive from logical key plus commit ID.

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

### `workspace`

A bounded private state transition based on one commit. Fields include repository, branch, base
commit/root, author principal/span, hashed capability identifier, isolation backend, process-scope
identifier, status (`open`, `publishing`, `committed`, `aborted`, `expired`), quotas, expiry, and final
commit/abort reason. The plaintext bearer capability is never stored.

Workspace records are operational evidence, not canonical state. Only their published commit/root is
visible through a branch. A captured tool span owns one writable workspace in the initial contract.

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
| `state_root` | Immutable `state_root` record after commit |
| `state_root_digest` | Digest of the root's canonical bytes for receipts/export |
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

Immutable inode value referenced from an inode-tree node. Its logical identity is stable `inode_id`;
its record identity also includes canonical content/version identity.

Fields:

- stable numeric or 128-bit `inode_id`;
- kind: regular, directory, symlink, device where supported;
- mode, owner, group;
- logical size;
- link count;
- atime, mtime, ctime with explicit update policy;
- device metadata where supported;
- introducing commit/evidence reference;
- content/extent root digest;
- xattr root/reference;
- deleted/open-unlinked state where applicable.

### `inode_version`

Optional immutable explanation/index record connecting one logical inode mutation to a commit,
previous value digest, and resulting immutable inode value. It accelerates blame/history queries but
is not the lookup mechanism for committed state.

### `dentry`

Immutable namespace entry or tombstone stored under the namespace root by
`(parent_inode, normalized_name_bytes)`.

The original byte name is retained. Name normalization and case sensitivity are repository-format
properties and cannot change in place.

### `dentry_version`

Optional immutable explanation/index record connecting one namespace mutation to its commit,
previous entry digest, and resulting immutable dentry/tombstone.

### `file_extent`

Immutable non-overlapping file extents ordered by file offset under an extent root. Each extent is
either a hole or references `(chunk, chunk_offset, length)`. Updating one file path-copies affected
extent-tree nodes; unchanged nodes/chunks remain shared.

### `symlink_data`

Small targets may be fields on inode records; a separate record is acceptable when uniform content
handling is simpler. Symlink reads never resolve the target in storage; path traversal does.

### `xattr`

Immutable xattrs key by inode and attribute name inside the metadata tree. Tombstones capture
deletion. Security-sensitive namespaces require explicit platform and policy handling.

## KV records

### `kv_entry`

Immutable `(namespace, key_bytes)` value/tombstone referenced by the commit's KV root, with optional
expiry, content type, value digest, and introducing commit evidence.

### `kv_version`

Optional immutable explanation/index record with commit, previous value digest, resulting value,
and expiry semantics. Expiration is a committed system mutation when it changes visible state;
wall-clock filtering alone must not make historical state ambiguous.

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

## Canonical state versus projections

Every successful commit writes atomically:

1. new immutable persistent-tree nodes and one `state_root`;
2. commit and ordered mutation/evidence records;
3. relations and request receipt;
4. branch-head update;
5. optional projection updates only when that projection is enabled.

The root is canonical. Mutation/version records explain how it changed; they do not define lookup by
ancestry. A materialized head is a disposable, root-keyed projection. Production may enable it only
after benchmarks show material benefit and rebuild/verification tests pass.

## Branching model

A branch is a mutable pointer to a retained commit. Creating a branch records its source commit and
copies no logical state. Reads start from the selected commit's root; there is no branch-local/base
fallback chain and no eager full-state materialization. Workspace overlays are transient write sets,
not branch layers. Merge creates a new root and multi-parent commit after explicit resolution.

`generation` may accelerate validation or ancestry indexing, but equality/range comparisons on
generation never prove ancestry. First-parent logs follow direct parent links; deeper operations may
add verified ancestor skip indexes after measurement.

## State roots

State roots are the primary current/historical lookup boundary and the integrity/comparison boundary.
A `state_root` has four versioned children:

- `namespace_root`: parent/name to stable inode identity;
- `inode_root`: inode identity to immutable metadata/content-root value;
- `extent_root`: inode/range to immutable extent/chunk references;
- `kv_root`: namespace/key to immutable value or tombstone.

The implementation may physically combine trees when measured, but logical root components and
canonical encoding remain independently verifiable. The algorithm defines:

- canonical ordering of filesystem and KV logical keys;
- canonical encoding of metadata and content references;
- inclusion/exclusion of volatile timestamps;
- tombstone treatment;
- persistent-node encoding, fanout, child ordering, and path-copy rules;
- hash algorithm and version;
- root-format upgrade and dual-root migration rules.

The Phase 1 proof implements a minimal content-addressed persistent tree. The root is computed before
publish and stored in the same transaction; a commit with a pending/missing root is invalid. Full or
sampled post-commit verification may be asynchronous, but canonical identity cannot be.

## Index plan

At minimum index:

- repository on every system table;
- unique branch name within repository;
- branch head and base commit;
- commit by branch/sequence and parent;
- state root/node by repository, format, kind, and digest;
- optional head projection by branch, source root, and logical key;
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
- immutable state nodes reachable from retained commit roots remain;
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
