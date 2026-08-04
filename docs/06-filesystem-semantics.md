# Filesystem semantics

## Scope

SurrealFS implements the filesystem behavior required by supported agent workloads through one Rust
semantic kernel. FUSE, NFS, direct SDK, and sandbox paths must resolve to the same rules.

The database stores logical filesystem state. Mount adapters handle kernel protocol details and cache
invalidation but cannot define alternate namespace semantics.

## Namespace model

A path is resolved one component at a time:

```text
(branch, root inode)
  + name -> dentry -> child inode
  + name -> dentry -> child inode
  ...
```

Persistent identity uses parent inode and raw name bytes, not full paths. This makes rename a bounded
namespace operation and preserves identity across moves.

Repository format fixes:

- case-sensitive or case-folded behavior;
- name normalization rules;
- maximum component and path lengths;
- supported byte encoding and invalid-byte behavior;
- root inode identity;
- reserved internal names.

These properties cannot change without an explicit repository conversion.

## Inodes and dentries

- Inodes carry object identity and metadata.
- Dentries map a name under a parent directory to an inode.
- Multiple dentries may reference a regular-file inode through hard links.
- Directories have restricted hard-link behavior to prevent cycles.
- Deleting a dentry does not necessarily delete the inode.
- Inode reclamation requires zero persistent links and no live open-handle requirement.

The root directory's link count and `.`/`..` behavior are specified explicitly and covered by tests.
Synthetic `.` and `..` need not be stored as ordinary dentries.

## Open handles

The daemon maintains process-scoped handles containing:

- repository and branch/session;
- inode identity and open generation;
- access mode and flags;
- current offset where protocol semantics require it;
- append behavior;
- lease/advisory lock state;
- reference count;
- whether the inode has become unlinked.

An unlinked open file remains accessible through existing handles. Its persistent state is retained
until handles close or recovery policy proves no live handles remain.

After daemon crash, client handles are invalid. On recovery, inodes marked open-unlinked are reclaimed
or restored according to durable handle/session records if durable handles are ever introduced. The
first release uses non-durable handles and documents reconnect behavior.

## Operation semantics

### Lookup

Resolve one dentry from branch, parent inode, and normalized name. Require parent to be a directory and
caller to have traversal permission. Negative lookup caching in mount adapters must be invalidated by
commits affecting the parent.

### Create regular file

Atomically:

- require target dentry absent unless flags allow existing;
- allocate stable inode ID;
- create inode with link count one;
- create dentry;
- update parent mtime/ctime and generation;
- record history and cause.

### Create directory

Additionally update parent and child directory link-count semantics. The initial mode, owner, group,
umask application, and timestamps are one commit.

### Read

Read a consistent inode generation and ordered extent view. Sparse regions return zero bytes. Hash
verification policy may verify every chunk, first read, sampled reads, or explicit integrity mode.

### Write

Streaming writes stage chunks before committing a new extent map. Overlapping extents are replaced
atomically in the visible state. Append determines the write position against the transaction's
current size and must serialize/conflict correctly with concurrent appenders.

### Truncate

Shrinking removes or shortens extents after the new size. Growing creates a sparse logical hole rather
than materializing zero chunks. Size, times, extent root, history, and artifact effects commit
together.

### Rename

Rename is one namespace transaction. It validates source and destination parents, cycle prevention
for directories, replacement type compatibility, non-empty destination directory rules, link counts,
and parent timestamps. Source removal and destination insertion are never separately visible.

Cross-repository rename is unsupported and returns cross-device semantics. Cross-branch rename is a
copy/merge workflow, not a filesystem rename.

### Link

Create a new dentry to an existing eligible inode and increment link count atomically. Directory hard
links are rejected except internal root semantics.

### Symlink

Store target bytes without resolving them. Resolution enforces maximum link traversal count and loop
detection. Relative targets resolve against the containing directory.

### Unlink

Remove one dentry and decrement link count. If count reaches zero with no live handle requirement,
mark inode and its current extents deleted through history and materialized-state removal. Chunk bytes
remain until repository reachability and retention allow GC.

### Remove directory

Require directory empty except synthetic entries, enforce root protection, remove dentry/inode, and
update parent link count atomically.

### Directory listing

Provide stable pagination over normalized name order under a consistent read view. Mount cookies must
encode enough state to reject or restart invalidated iteration safely. Do not expose database record
ordering as a protocol guarantee without versioning it.

### Metadata update

`chmod`, `chown`, `utimens`, and supported device metadata apply permission rules and update ctime.
Policy can restrict ownership or executable-bit changes made by agents.

### Extended attributes

Support a declared namespace subset. Large attributes may use content addressing. Security and system
namespaces require explicit platform behavior; unknown namespaces must not be silently accepted if the
mount would imply stronger semantics.

### `fsync`

`fsync` establishes the documented durability boundary for prior writes through the handle. In
durable-per-commit mode, writes may already be durable; `fsync` still waits for all relevant commit
receipts and storage sync. It must not be implemented as copying database files.

## Timestamps

Store nanosecond-capable logical timestamps with clear source and update policy:

- `mtime`: content or directory membership change;
- `ctime`: inode metadata or link change;
- `atime`: configurable `noatime`, `relatime`, or strict mode;
- commit creation time: audit metadata, not causal order.

Atime-only writes can create unacceptable amplification. Production default should be `relatime` or
`noatime`, with strict behavior opt-in.

Clock rollback cannot alter commit order. Branch sequence and commit ancestry are authoritative.

## Permissions

The kernel receives authenticated subject, groups, capabilities, and policy context. It evaluates:

- traversal permission for every directory component;
- read/write/execute bits;
- ownership and privileged operations;
- mount/session read-only status;
- repository and branch policy;
- agent-specific capability restrictions.

Do not rely exclusively on SurrealDB table permissions for POSIX behavior. Database permissions
protect records; filesystem permissions are domain semantics.

## File content layout

### Chunking

Start with fixed-size chunks for simplicity, with a versioned default selected by benchmark. A 64 KiB
starting point aligns with the current SurrealKV block default but is not automatically optimal.

Content-defined chunking may improve deduplication across inserted prefixes or similar artifacts but
costs CPU and complicates random writes. It should be evaluated on actual agent files before adoption.

### Extents

Extents are sorted, non-overlapping, and cover only stored ranges. Holes are implicit or explicit by
format version. Extent updates enforce:

- positive lengths;
- no integer overflow;
- chunk bounds;
- no overlapping visible ranges;
- file size consistent with highest logical byte;
- deterministic ordering and digest.

### Small files

An inline-content optimization can avoid separate chunk records below a measured threshold. It must
preserve content hashes and export semantics. Avoid adding it before the baseline is measured.

## KV semantics

Agent KV state shares repository/branch/commit semantics with files:

- get, set, delete, scan;
- optional compare-and-set by generation or digest;
- namespaces and byte keys;
- optional expiry represented by committed transitions;
- value content type and digest;
- history and diff support.

A commit may change files and KV entries atomically.

## Branch and historical reads

Mounts identify a branch head or immutable commit:

- read-write branch mount follows commits produced by its session and observes current head;
- read-only commit mount never advances;
- snapshot mount resolves once to its target commit;
- historical lookup uses state versions and branch ancestry;
- branch-head changes invalidate affected kernel caches through commit notifications.

## Merge behavior

The first merge implementation is explicit and conservative:

- find merge base from commit ancestry;
- compare logical keys and content digests in base/source/target;
- auto-apply one-sided changes;
- detect namespace, metadata, content, delete/modify, and type conflicts;
- accept caller-selected `ours`, `theirs`, or path-specific resolutions;
- write a merge commit with all parents and resolution evidence.

Semantic source-code merges can be layered above this protocol. The filesystem kernel should not
silently invent content resolutions.

## Platform deviations

Maintain a versioned compatibility matrix for Linux FUSE, macOS NFS, direct SDK, and sandbox access.
Candidates requiring explicit declaration include:

- device nodes and special files;
- mandatory locking;
- mmap coherence;
- direct I/O;
- filesystem notifications;
- ACL variants;
- birth time;
- case folding;
- sparse-file reporting;
- atomicity of very large writes split across commits;
- Windows path and permission behavior.

An unsupported operation returns a specific error. Silent approximation is allowed only when the
contract declares it.

## Caching

Cache keys include repository, branch/commit view, logical key, and generation. Commit notifications
carry affected inode/dentry/KV summaries for invalidation. A cache must not serve current data for a
historical view or data from another tenant/repository.

Mount caches are treated as advisory. Correctness must survive cache eviction, duplicate requests,
reordering allowed by the protocol, and daemon reconnect.

