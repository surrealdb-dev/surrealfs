# Migration from AgentFS

## Migration principle

Migration preserves evidence; it does not fabricate it.

The existing AgentFS v0.4-style store is understood as an overwrite-oriented filesystem and
key-value database with tool-call events. It can provide current file/KV state and some recorded
events, but it does not necessarily retain the immutable mutation history, branch graph, inode
lineage, external inputs, or causal boundaries SurrealFS requires.

The importer therefore creates a **synthetic genesis import commit** for current state. Every
imported fact records its confidence and source locator. Historical events may be preserved as
legacy observations, but they are not asserted to have caused file content unless the source
contains verifiable linkage.

## What users are promised

After a successful import:

- current visible files, directories, symlinks, supported metadata, and KV entries match the
  source at the captured boundary;
- imported content hashes and counts have been verified;
- source tool-call/event records remain queryable with their original identifiers when possible;
- all new SurrealFS mutations have full native causal semantics;
- the import can be repeated idempotently into an empty target;
- a manifest explains every omitted, transformed, ambiguous, or unsupported source feature.

The importer does **not** promise:

- reconstruction of earlier overwritten file or KV values;
- accurate original commit times where no commit existed;
- causal links between legacy tool calls and state changes based only on timestamp proximity;
- byte-identical inode numbers or database record IDs;
- deterministic replay of past agent behavior;
- preservation of source database page layout or engine-specific indexes.

## Migration architecture

Migration is an offline or snapshot-consistent pipeline:

```text
AgentFS source
   |
   | read-only exporter at captured boundary
   v
versioned neutral migration bundle
   |  manifest + records + blobs + warnings + checksums
   v
SurrealFS staging importer
   |
   | schema validation + hash checks + invariant reconstruction
   v
hidden imported repository
   |
   | state-root and semantic verification
   v
atomic publication of branch head
```

Source export and target import are separate commands. This gives a stable artifact to inspect,
allows air-gapped transfer, and prevents target logic from depending permanently on SQLite/Turso
internals.

## Pre-migration inventory

Before code is written, the migration owner records for each supported source version:

- exact schema and migration number;
- database engine and journal mode;
- whether a snapshot-consistent online read is possible;
- path encoding and uniqueness rules;
- file content representation and size limits;
- directory and symlink representation;
- stored timestamps, permissions, xattrs, and ownership;
- KV namespace/key/value encoding;
- tool-call/event schema and ordering guarantees;
- foreign-key/index constraints actually enforced;
- deletion/tombstone behavior;
- known corruption or partial-write cases;
- maximum observed repository sizes.

The exporter refuses unknown schemas by default. `--allow-unknown-source` may only produce an
inventory report, not a publishable import.

## Neutral migration bundle

The bundle is append-friendly, streamable, checksummed, and versioned independently of the final
SurrealFS logical export. It contains:

```text
manifest.json
  bundle_version
  exporter_version
  source_product/version/schema
  source_database_fingerprint
  capture_started_at/capture_finished_at
  consistency_method
  source_counts
  warnings[]

records.ndjson or framed binary records
  Repository
  FileEntry
  KvEntry
  LegacyEvent
  LegacyToolCall
  SourceMapping

blobs/<pack files>
index
checksums
```

Every record has a canonical source locator and content checksum. The final manifest checksum
covers record order and pack indexes. The format never contains executable SQL.

## Identity mapping

IDs that already meet uniqueness and safety constraints are preserved in a `source_id` field.
Native SurrealFS IDs are derived deterministically from:

```text
H(import_namespace, source_database_fingerprint, entity_type, canonical_source_id)
```

This makes retry/import comparison stable without letting hostile source strings become table or
record identifiers. A `source_mapping` record maps source type/ID to target record ID and records
the transformation version.

Repository ID may be caller-supplied for organizational placement, but the same bundle plus target
repository ID must resolve to the same native entity IDs.

## Current-state mapping

| AgentFS source | SurrealFS target | Rule |
|---|---|---|
| Database/workspace | `repository` | One imported repository unless explicitly partitioned |
| Current file row | `inode` + `dentry` + extents/chunks | Content hashed and chunked; metadata preserved where meaningful |
| Directory/path prefix | explicit directory inode/dentries | Synthesize missing parent directories with an import warning |
| Symlink | symlink inode/content | Preserve target bytes; never resolve against host during import |
| KV row | KV mutation in genesis import commit | Preserve namespace/key/value bytes and source update time |
| Tool-call row | `tool_call` with `capture_quality=IMPORTED` | Preserve observed fields and original ordering |
| Event row | legacy span event/artifact | No causal edge to commits without source evidence |
| Timestamp | source-observed timestamp attribute | Import commit time remains the actual import time |
| Deleted/overwritten row | nothing unless tombstone exists | Report unrecoverable history explicitly |

## Synthetic genesis structure

The imported repository begins with two commits if useful:

1. `repository_root`: an empty, tool-created root marking native repository creation.
2. `legacy_import`: one synthetic commit whose ordered mutations materialize captured AgentFS
   current state.

The second commit includes:

- `kind = IMPORT`;
- exporter/importer versions;
- bundle checksum;
- source database fingerprint;
- source snapshot boundary;
- counts and warnings;
- `capture_quality = IMPORTED_CURRENT_STATE`;
- state root;
- no tool-call author;
- an authenticated migration principal as actor.

It must not use a past timestamp as its commit time. Source timestamps live on imported entities
with labels such as `source_created_at` and `source_updated_at`.

## Path and inode reconstruction

AgentFS paths may not contain persistent inode identity. The importer constructs it
deterministically:

1. Parse source paths using source encoding rules.
2. Reject NUL, absolute/relative ambiguity, root escape, and duplicate canonical identity.
3. Sort by raw canonical path bytes for deterministic construction.
4. Create the root directory inode.
5. Create explicit parent directories; mark any synthesized parents.
6. Create one inode per source entry unless hard-link identity is provable.
7. Add dentry records and content extents.
8. Preserve supported mode, time, and xattr metadata; list losses.
9. Re-walk the constructed tree and calculate a state root.

Two source paths with identical content do not become hard links merely because their hashes
match. Chunk storage may deduplicate content without changing filesystem identity.

Case and Unicode collisions are resolved only through an explicit migration policy:

- `strict`: fail the import;
- `escape`: produce deterministic escaped names and a mapping report;
- `case-sensitive-target`: preserve raw bytes if the target repository supports it.

Default is `strict`.

## Legacy event treatment

Legacy events are valuable evidence but often lack complete causality. Each gets:

```text
capture_quality = CAPTURED | IMPORTED | INFERRED
source_order
source_timestamp
source_payload_hash
source_locator
```

The importer only creates a `caused` edge if the source explicitly and unambiguously stores the
relationship. Timestamp adjacency may be computed later as an analytical hypothesis, represented
as an `inferred_relation` with method/version/confidence, never as captured truth.

If a legacy event embeds an output artifact identical to an imported file, SurrealFS may create a
`content_matches` edge. It must not relabel it `produced` without stronger evidence.

## Migration phases

### Phase M0: discovery and fixtures

- collect anonymized databases across every supported source schema and size class;
- write a read-only schema inspector;
- define loss and ambiguity categories;
- publish the neutral bundle specification;
- create corrupted and partially migrated fixtures.

Exit: every source field has a preserve/transform/drop decision and a test fixture.

### Phase M1: deterministic exporter

- open the source read-only;
- establish a snapshot boundary using the source engine's supported mechanism;
- stream rows in deterministic order;
- copy and checksum blobs without unbounded memory;
- emit counts, schema fingerprint, warnings, and terminal checksum;
- prove two exports of unchanged state are logically identical except allowed capture metadata.

Exit: exporter never mutates source and passes malformed-source tests.

### Phase M2: staging importer

- authenticate the migration principal;
- create an invisible target repository/import session;
- stream-validate bundle records and quotas;
- deterministically construct chunks, inodes, graph records, and source mappings;
- write the synthetic import commit;
- make retries return the same import receipt.

Exit: interruption at any record leaves no visible branch and can resume or restart safely.

### Phase M3: verification

- compare file and KV counts;
- compare each content/value hash;
- compare path set and supported metadata;
- re-materialize and walk the target tree;
- recompute commit/state roots;
- validate graph endpoints and tenant/repository scope;
- compare legacy record counts and source mappings;
- produce a signed human- and machine-readable report.

Exit: zero unexplained differences. Accepted losses are enumerated, not hidden in totals.

### Phase M4: shadow operation

- keep AgentFS authoritative;
- periodically export/import into a disposable SurrealFS target;
- replay read-only workload queries and compare results;
- measure export lag, import duration, disk amplification, and query latency;
- have users inspect histories, files, and explanation results.

Exit: target correctness and operational budgets hold for representative repositories.

### Phase M5: cutover

1. Announce and enforce a source write freeze.
2. Create the final source-consistent bundle.
3. Import and verify into the production target.
4. Record final source fingerprint and target receipt.
5. Point clients/mounts to `surrealfsd`.
6. Run smoke reads, a native test commit, reopen, and provenance query.
7. Keep the source read-only for the rollback window.

Exit: all clients use SurrealFS; monitoring and support sign off.

### Phase M6: retirement

- retain the source according to approved retention and customer policy;
- retain the verified bundle as migration evidence;
- prevent accidental dual writing;
- remove compatibility code only after support obligations end.

No source files are deleted automatically by the migration command.

## Rollback

Rollback during the window means:

- stop SurrealFS writes;
- export native commits created since cutover;
- decide whether those changes can be translated back, manually applied, or preserved as a
  separate archive;
- repoint clients to the untouched read-only source only after acknowledging potential divergence.

There is no magical bidirectional migration. Once native SurrealFS writes occur, the old model may
not represent forks, provenance, or atomic file+KV commits. A canary cohort and short cutover smoke
phase reduce this risk.

## Verification report example

```text
source fingerprint: sha256:...
bundle checksum:    sha256:...
target repository:  repository:...
import commit:      commit:...

files:        12,491 / 12,491 matched
directories:     832 / 832 matched (3 synthesized parents)
symlinks:          8 / 8 matched
kv entries:    2,044 / 2,044 matched
legacy calls: 18,201 / 18,201 preserved
content bytes:  9.4 GiB / 9.4 GiB verified

losses:
  12 source events had invalid timestamps; raw values retained
  source had no hard-link identity; entries imported independently
  overwritten history unavailable in source

result: PASS_WITH_DECLARED_LOSS
```

## Migration release gates

- Source read-only guarantee is tested and documented.
- Unknown source schemas fail closed.
- Interrupted export/import is retry-safe.
- Importing the same bundle twice cannot duplicate visible data.
- Every path/content/KV value is hash-verified.
- No inferred causality is returned as captured provenance.
- Large repositories stream within memory limits.
- Malicious archives cannot escape scope or exhaust unbounded resources.
- Cutover and rollback drills are completed using production-sized copies.
- Users receive and approve the declared-loss report before authoritative cutover.
