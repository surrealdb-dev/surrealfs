# Prior-art comparison: AgentFS, Cloudflare `dofs`, and ContextFS

## Purpose

This document is the evidence base for the normative statements made elsewhere: the AgentFS
"verified baseline limits" list in `RUST_SDK_PLAN.md`, the Cloudflare `dofs` adoption matrix in
[system architecture](03-system-architecture.md), and the differentiator claim in the plan's
Objective. It records what these systems actually do, from source review rather than from their
published specifications — a distinction that has mattered every time: AgentFS's README claims
WAL-based time-travel forking that does not exist in its source, and ContextFS's headline merge
is narrower than its README implies.

Sources reviewed:

| System | Path | Revision |
|---|---|---|
| AgentFS | `/Users/kfarhan/workspace/projects/agentfs` | `0a014eb`, v0.6.4, SPEC 0.4 |
| Cloudflare `dofs` | `/Users/kfarhan/workspace/projects/computer` | `76d9e75` |
| ContextFS (AgentVFS) | `/Users/kfarhan/workspace/projects/ContextFS` | `1aaa360`, `thustorage/ContextFS` |

The `dofs` package tree is byte-identical between `76d9e75` and the `cfa51ba1` revision pinned in
`RUST_SDK_PLAN.md`; the difference between those checkouts is CI and documentation plumbing only.

## Summary: AgentFS and `dofs`

The two SQLite-backed systems are the same genus and different species. Both are POSIX-style filesystems encoded
in SQLite tables using inode and directory-entry indirection. The divergence is concentrated almost
entirely in one layer — content storage — and it is causal rather than incidental: `dofs` rebuilt
the content layer because its sync protocol required content addressing, not because it disagreed
with AgentFS about filesystem semantics.

| Layer | AgentFS | `dofs` |
|---|---|---|
| Metadata | `fs_inode`: full POSIX, with `nlink`, `uid`, `gid`, `rdev`, and separate `atime`/`mtime`/`ctime` plus `_nsec` columns | `vfs_nodes`: `mtime` only, a `type` enum, plus sync columns `rev`, `manifest_hash`, `stub_size`, `mount_root` |
| Directory | `fs_dentry` with a surrogate `id` | `vfs_dirents` with composite primary key `(parent_inode, name)`, `WITHOUT ROWID` |
| Content | `fs_data(ino, chunk_index, data BLOB)`: position-keyed, bytes inline, no hashing, no deduplication, 4 KiB default | `vfs_chunks(inode, idx) -> hash` plus `vfs_blobs(hash)` and `vfs_blob_bytes(hash)`: SHA-256 content-addressed and deduplicated, 512 KiB fixed |
| Manifests | none | `vfs_manifests(hash, encoded)`, a per-file chunk list |
| Sync bookkeeping | none; delegates to Turso database replication | `vfs_changes` delete tombstones, `_vfs_watermark`, `_vfs_fetch_cursor` |
| Overlay and copy-on-write | `fs_whiteout`, `fs_origin` | none |
| Agent domain | `tool_calls`, `kv_store` | none |
| Mount surface | FUSE and NFSv3, both vendored | FUSE only |
| Language and maturity | Rust, TypeScript, Python, Go; published SPEC 0.4; `agentfs-sdk 0.6.4`, beta | TypeScript; `0.0.0`, private, preview only |

## The content layer, and why `dofs` diverged

Cloudflare evaluated AgentFS explicitly and rejected it on a single blocker, stated in their
`docs/03_filesystem_schema.md`:

> The blocker is the data table. AgentFS keys chunks by `(ino, chunk_index)` with raw bytes inline;
> we key chunks by `sha256(bytes)` and dereference through a manifest. The two are not slot-in
> compatible — our sync protocol is *built* on hash-addressed chunk dedup and manifest sharing
> across paths, both of which AgentFS explicitly does not provide.

They retained AgentFS's metadata vocabulary — POSIX mode bits, `nlink`, nanosecond timestamp columns
— and declined a runtime dependency on `agentfs-sdk`. AgentFS's specification confirms the gap is
deliberate: version history, content deduplication, and file checksums are all listed as optional
extension points rather than requirements.

## Where truth lives

This is the deeper architectural split, and it constrains each design more than the schema does.

AgentFS is a host-side SDK over a local SQLite file, serving FUSE or NFS from the host; the
application owns the database. `dofs` places truth server-side inside a Durable Object, and
containers are clients that synchronise against it. `dofs`'s container-side mirror is an in-memory
SQLite rebuilt on every start, so it is a cache rather than a replica.

## What AgentFS and `dofs` lack

Both are live mutable state. Neither has commits, immutable roots, snapshots, or history.
(ContextFS does — see below. This section is about the two SQLite-backed systems.)

In AgentFS this is explicit: history is a documented non-goal. Its README nevertheless claims the
write-ahead log "enables snapshotting and time-travel forking by capturing every filesystem change";
no such capability exists in the repository, nothing reads the WAL, and the documented snapshot
mechanism is copying the database file. In `dofs`, `rev` is a monotonic sync cursor rather than a
version, and `vfs_changes` is constrained to delete tombstones. There is no state-root or commit
table in either system.

Neither provides causal attribution from an action to a file version. AgentFS's `tool_calls` is a
sibling table with no foreign key or correlation column reaching the filesystem tables.

Their concurrency stories differ in kind. `dofs` genuinely admits concurrent writers across
containers and resolves them by silent last-write-wins, which Cloudflare documents candidly as "the
same semantics as a shared NFS mount without locking, or an S3 bucket without conditional PUTs".
AgentFS has no conflict-resolution story because it structurally forbids concurrency: its connection
pool is capped at one connection, so every call including reads serialises. Neither offers
expected-head or compare-and-swap semantics.

Immutable roots, explicit publication, expected-head conflicts, and action-to-commit attribution
are not features either of these two chose against; they are features neither has.

It would be a mistake to read that as the field being empty. ContextFS has commits, refs,
branches, rollback, merge, and garbage collection, and is reviewed below precisely because it is
the closest prior art on versioning. What survives contact with it is a narrower and more
defensible claim, stated at the end of this document.

## AgentFS: findings that bear on parity

Recorded here because they determine what "parity" can mean. Fixed decision 8 in `RUST_SDK_PLAN.md`
governs which of these SurrealFS reproduces and which it deliberately does not.

- Durability is off by default. `PRAGMA synchronous = OFF` is set on every connection at open, and
  `fsync` is emulated by flipping to `FULL`, running an empty `BEGIN; COMMIT;`, and flipping back.
- Explicit transactions exist at only eight call sites. `unlink`, `rmdir`, `link`, `mkdir`, `mknod`,
  `symlink`, `chmod`, `chown`, and `utimens` run in autocommit. `unlink` spans five or six
  independent transactions, so a crash mid-sequence leaves an orphaned inode or a dangling dentry,
  and `link` can leave a wrong link count.
- All access serialises on a single pooled connection, semaphore-gated with a 30-second timeout.
  FUSE sets an infinite kernel-cache TTL justified by being the only writer.
- Open-unlinked handles are broken: the inode row is deleted once `nlink` reaches zero with no
  open-handle refcount, so reads through a still-open descriptor return zero bytes.
- NFS `COMMIT` is unimplemented, so `fsync` over the macOS NFS path is unavailable.
- Extended attributes and file locking return `ENOSYS` through FUSE.
- Overlay copy-up is whole-file and eager, and triggers even on a read-only `open`.
- The tool-call audit log is opt-in and usually empty: the shipped MCP server, FUSE, NFS, and
  sandbox paths never write `tool_calls`.
- Whole-database encryption is mutually exclusive with sync because the upstream Turso sync builder
  hardcodes `encryption: None`.

### Specification and implementation disagree

`fs_whiteout` carries `parent_path` and an index in `SPEC.md` and in the Go SDK, but not in the Rust
SDK, which loads all whiteouts into memory instead — so the two SDKs write incompatible shapes.
`fs_overlay_config` and `tool_calls.status` exist in code but not in the specification. Schema
version detection sniffs `PRAGMA table_info` rather than reading the `fs_config['schema_version']`
value AgentFS itself writes. The Go SDK is built on `modernc.org/sqlite` rather than Turso.

Consequence, and the reason this section exists: **treat `SPEC.md` as intent and the pinned Rust
source as truth.** Parity fixtures are generated from observed Rust-SDK behavior and record which
SDK produced them.

## `dofs`: findings that bear on the adoption matrix

- Received content identity is not verified anywhere. `stageBlob` documents that it "trusts the
  caller", and both receive paths pass wire bytes through unchecked. Its upsert uses
  `DO UPDATE SET bytes = excluded.bytes`, so a bad pair overwrites an already-correct payload, where
  the local write path uses the safer `DO NOTHING`. This is why the adoption matrix says *adopt and
  strengthen* rather than simply adopt.
- Manifests are not the transfer unit their rationale implies. Change entries carry chunk lists read
  from `vfs_chunks`, and the buffered-write path sets `manifest_hash = NULL`, disabling the manifest
  fast path for any file last written through FUSE.
- Hardlinks work correctly locally, with refcount-gated reap and `nlink` derived from `vfs_dirents`.
  Identity is lost only in transfer, where the coalescer emits one entry per name and the apply side
  calls `writeFile` rather than `link`.
- Atomicity is asymmetric by design. The push receiver applies a whole batch in one transaction; the
  pull path is a sequence of independently committed mutations, because a synchronous transaction
  cannot be held across network I/O. Cursor advancement is explicitly non-atomic with apply.
- The POSIX surface is thinner than the schema suggests. Extended attributes are accepted and
  discarded, `chown` and `utimens` write to a process-memory sidecar that never reaches SQLite or
  sync, and `mknod` is unimplemented.
- `vfs_changes` is never pruned, so the tombstone table grows without bound.
- The published DDL in their own `docs/` has already drifted from the shipped schema.

## Techniques worth taking, and the reason

The normative decisions live in the [adoption matrix](03-system-architecture.md#cloudflare-dofs-adoption-decisions).
The short version: content addressing and staged bytes before a short metadata transaction are worth
adopting and are already in the SurrealFS v1 design. Identity verification is worth adopting and
strengthening, because `dofs` does not do it. Live-state path/revision sync, silent last-write-wins,
whole-file assembly, and hardlink flattening on the wire are rejected, because immutable commits and
expected-head conflicts already provide stronger guarantees.

Chunk size is the one parameter worth measuring rather than inheriting. AgentFS uses 4 KiB, `dofs`
uses 512 KiB, and the SurrealFS v1 choice of 256 KiB sits between them. Benchmarks should sweep the
whole range; the AgentFS end is 64 times smaller than the current choice and is the likelier source
of surprise on small-file workloads.

## ContextFS (AgentVFS)

The closest prior art on versioning, and the reason the claim above is scoped to two systems
rather than to the field. A ~22k-line C++ FUSE daemon over a hand-built content-addressed store,
from `thustorage`. Eight weeks of history at the reviewed revision, shipping an installer, three
mount backends, and Claude Code / Codex skills.

### What it genuinely has

Verified in source, not taken from the README:

- **Real commit objects.** `src/cas/commit.h` — tree hash, multiple parents, session id,
  timestamp, label — serialised into a BLAKE3-addressed object store as `objects/<2>/<64>` at
  mode `0444`. Trees reference blobs by hash. This is a Git-shaped DAG, not snapshot-as-copy.
- **Refs and branches.** One file per branch, written tmp → `fsync` → `rename` → parent `fsync`,
  and reads fail closed on a malformed ref. Branch creation is copy-on-write over a shared
  immutable base map, so it costs O(churn) rather than O(tree) — the same property as our fork.
- **Rollback** to a hash or a label, with a correct interaction with retention: a compacted
  checkpoint reports "checkpoint compacted by retention policy" rather than failing obscurely.
- **Three-way merge** with a real merge base (BFS ancestor sets, deterministic tie-break).
- **Mark-and-sweep GC** with a two-second mtime age fence so a concurrent publisher's
  not-yet-referenced object is never swept — the same hazard our grace period addresses.
- **cgroup v2 per-agent routing**, with a genuine `routing_fence` (eBPF generation counter,
  pinned PID namespace, inotify on the parent dir because kernfs does not deliver
  `IN_DELETE_SELF` for cgroup rmdir). This is careful work.
- **Telemetry** via eBPF, bpftime, fanotify, ptrace, and `LD_PRELOAD`, emitting NDJSON.
- **A benchmark harness** including a do-nothing FUSE server as a performance floor, and
  agent-simulation CSVs that already compare against `agentfs` and `branchfs`.

### Where it stops, and why the difference matters

**There is no query layer of any kind.** No SQLite, no embedded database, no index. Persistence
is flat files: CAS objects, one-line ref files, an append-only text `index.log`, and NDJSON
telemetry.

**It cannot answer "which tool call produced this file version."** The link is *expressible* —
an `AgentStateRecord` has `kind = ToolCall` and an `fs_commit` hash — but three things stop it
being answerable:

1. granularity is per-commit; nothing ties an action to a path, a blob, or a byte range;
2. there is no reverse index and no listing verb — reads are `describe(state_id)`,
   `latest(agent, branch)`, and a parent-chain walk, so answering it means hand-scanning a log
   the design doc itself calls best-effort and losable;
3. the telemetry stream carries no commit hash or state id at all, so a syscall can be joined to
   a branch and a wall-clock instant and nothing more.

Their own roadmap lists this as unbuilt: *"correlate each call with the file changes it caused."*
There is not even a `log` command — you can roll back to a label but cannot list history.

**The in-memory tree is truth, not a cache.** Namespace, metadata, and hash pointers live in a
process-memory map; committed *bytes* are read back from the CAS. Restart recovery from refs
exists, but there is no journal, so everything since the last checkpoint is lost on a crash, and
the roadmap's promised "written durability contract" does not exist in the repository. Memory is
unbounded — cold-state spill is unimplemented, and the authors flag 100-branch fan-out as a
multiplicative blow-up.

**Content is stored as whole-file blobs with no chunking.** A one-byte edit to a 1 GB file
materialises and stores a fresh 1 GB blob. We chunk at 256 KiB and share by digest.

**Merge is narrower than it first appears.** Pairwise only — N-way fan-in is an open roadmap
decision. Comparison is entry-level (kind, mode, hash), so two branches editing different lines
of one file conflict. And a conflict surfaces as a flat list of path strings with the merged tree
discarded entirely: no base/ours/theirs, no conflict type, no partial merge.

**Branches and telemetry are effectively Linux-only.** The branch machinery is portable, but
routing is not: off Linux every PID resolves to the main branch, so the macOS and Windows
adapters serve one view.

## What this leaves as the difference

Against ContextFS specifically, versioning is not the differentiator — it has commits, branches,
rollback, merge, and GC, and its cgroup routing is ahead of anything we have planned.

The differences that survive are narrower and, stated honestly, these:

- **Queryable action-to-byte provenance.** Ours is a graph traversal over stored relations —
  mutation to commit to span to tool call — returning an answer per *path*. Theirs is an opaque
  blob journal with no index, no reverse lookup, and no path granularity, and their own roadmap
  says the correlation is future work.
- **Durability by default.** Our state is durable first and cached second; theirs is resident
  first with checkpoint-or-lose semantics and no journal for in-flight work.
- **One transaction spanning files and KV**, with expected-head compare-and-swap. They have no
  transaction boundary at all — publication is a ref file rename.
- **Content-defined storage.** Chunked and shared by digest, so a small edit to a large file
  costs a chunk rather than a copy.

Against AgentFS and `dofs` the versioning claim still holds, because neither has any.
