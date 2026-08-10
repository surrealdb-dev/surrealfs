# Mount semantics: inode identity, time, and when a write becomes a commit

## Purpose

Three questions have to be answered before a FUSE or NFS adapter is written, because all three
are decided *below* the wire format and both adapters inherit whatever we choose:

1. Where does the `st_ino` a client sees come from?
2. Where do `mtime`/`atime`/`ctime` come from, given that a state root deliberately carries no
   clock?
3. When does a write become a durable commit?

Each was verified against AgentFS, Cloudflare `dofs`, and ContextFS — implementation, not
README. This document records what they do, what we do, and where we are provably worse.

## Question 1 — inode identity

| | AgentFS | `dofs` | ContextFS | SurrealFS |
|---|---|---|---|---|
| source | stored `fs_inode.ino` | stored `vfs_nodes.inode` | libfuse nodeid | synthesized per mount |
| survives rename | yes | yes | yes, in-mount | **yes**, in-mount |
| survives remount | yes | yes | **no** | **no** |
| reuse after delete | never (AUTOINCREMENT) | never (AUTOINCREMENT) | delegated to libfuse | **never** |

AgentFS and `dofs` both store an allocated inode, so identity is a column and survives
everything. That is the conventional design, and it is available to them because both accept an
allocated-identity data model.

We deliberately do not: a path *is* the identity, which is what keeps a state root a pure
function of content. ContextFS made the same choice and lands in the same place — its
`WorkingTreeEntry` is `{kind, hash, mode}` with no inode field, and `cas_getattr` never assigns
`st_ino` on any path. What its users observe is libfuse's own nodeid.

### Correction to an earlier claim

An earlier note said a rename changes a file's inode number here. **That is wrong.**
`MountKernel::rename` calls `InodeTable::rename`, which moves the mapping and keeps the number,
and a test asserts the renamed subtree keeps its inodes. Renames through the mount behave
conventionally.

The genuine limits are narrower and both are shared with ContextFS:

- **Numbers do not survive a remount.** They are allocated per mount from a counter. A client
  that caches an inode across an unmount/remount is holding a stale number. NFS clients cache
  file handles aggressively, so the adapter must derive its handle from the path digest rather
  than the inode number, or an unmount becomes silent misdirection.
- **A mount is snapshot-isolated.** It never re-reads head, so a rename by another surface is
  invisible to it. This is a coherent model — the mount sees its own private view — but it must
  be stated, not discovered.

### The finding that changes the adapter

libfuse's high-level API **overwrites `st_ino` with its own nodeid unless the mount sets
`use_ino`**. Of the three systems, only `dofs` sets it (`options.ts:65`). AgentFS plumbs
`stats.ino` all the way to `fillattr` and ContextFS has an entire `InodeMap` type, and in both
cases userspace may still not see the intended number.

This applies to libfuse's **high-level**, path-based API, which is what ContextFS and `dofs` both
sit on. It does *not* apply to `fuser`, the maintained Rust crate we use: `fuser` is a low-level
binding that speaks the kernel protocol directly, every reply carries an explicit `FileAttr.ino`,
and there is no `use_ino` option because no intermediate layer has an opinion to override.

So the requirement on our adapter is a discipline rather than a flag — fill `ino` from the table
on every reply, because with no layer in between, a zero or a stale value is nobody's job to
catch. An earlier draft of this document stated `use_ino` as a requirement on our own adapter;
that was wrong, and would have sent a reader hunting for an option `fuser` does not have.

A second trap, from `dofs`: it reports `ino: 0` for a file between create and release
(`stat.ts:46-48`, conceded in its own comment). With `use_ino` on, every concurrently-pending
new file stats as inode 0 at once, so `find -samefile`, hardlink detection, and tar dedup
conflate them. We allocate the inode at `create`, before any write, so we do not have this
window — and a test should pin that, because it is cheap to regress.

## Question 2 — timestamps

| | AgentFS | `dofs` | ContextFS | SurrealFS |
|---|---|---|---|---|
| mtime | stored, +nsec | stored | **none — always 0** | derived from commit |
| atime/ctime | stored, +nsec | aliased to mtime | **none — always 0** | see below |
| `utimens` persists | yes | no (process memory) | no (pure no-op) | no (process memory) |
| `chown` persists | yes | no (process memory) | no (pure no-op) | yes (in `Meta`) |

**No prior-art system derives mtime from commit time.** Ours is the only one that does, and it
is a strict improvement on two of the three.

ContextFS is the cautionary case. Its timestamps are not a design choice — they are the residue
of a `memset` that no code path ever overwrites, so every file reports 1970-01-01 forever, and
`cas_utimens` is a no-op that returns success while changing nothing. Every mtime comparison on
that filesystem is a tie, which breaks `make`, `ninja`, and `rsync` without `--checksum`. It
*has* the data to fix this — `CommitData::timestamp_ns` exists — and simply never wires it to
`getattr`. That is precisely the wire we did run.

`dofs` stores one `mtime` column and aliases atime, ctime, and birthtime to it, conceding in a
header comment that the rest "don't map onto our content-addressed store". Its `chown` and
`utimens` write to a process-heap `Map` and are lost on daemon restart and invisible to every
non-FUSE consumer.

### What we owe

Our derivation is sound but incomplete:

- **`ctime` and `birthtime` are not distinguished.** `dofs` aliases them and says so; we should
  do better where it is free. `ctime` (inode change time) is derivable from the same mutation
  log as mtime — any `SetMeta` or rename touches it. `birthtime` is the *first* commit that
  created the path, which `first_commit` already answers.
- **`utimens` does not persist.** Storing a time in `Meta` would put a clock in the state root
  and break both reproducibility and the reference-model cross-check, so we will not. Accepting
  it into the mount's process-lifetime time cache matches `dofs` and is honest if documented;
  silently returning success while changing nothing, as ContextFS does, is not.
- **atime is not tracked.** AgentFS stores it but never updates it on read, so it behaves like a
  permanent `noatime` mount. Reporting mtime for atime and documenting the mount as `noatime` is
  the same observable behaviour with less machinery.

## Question 3 — when a write becomes a commit

| | AgentFS | `dofs` | ContextFS | SurrealFS |
|---|---|---|---|---|
| write lands | per-op autocommit | buffer → release | buffer → memory | staged in workspace |
| creates a version | no | `rev` bump, no history | yes, on checkpoint | yes, on publish |
| trigger | every op | close/fsync/release | **explicit only** | **explicit only** |
| buffer cap | n/a | 256 MiB/file, `EFBIG` | 64 MiB **declared, never checked** | 256 MiB/file + 1 GiB/workspace, `EFBIG` |
| `fsync` durable | **no** | yes | **no** | no |

The policy — explicit publication only — is fixed decision 9 and the research supports keeping
it. AgentFS's alternative is to commit on every operation, which makes a version history of
editor flushes rather than of agent actions.

But the policy has a cost that only shows up in the buffer row, and on that row we *were* the
worst of the four — the caps in that column are the fix described below, added in response to
exactly this comparison.

`Workspace::staged` is a `HashMap<ChunkDigest, Vec<u8>>` cleared only on publish or abort, with
zero bound. A mount that never publishes on its own is precisely the workload that grows it
without limit: a long agent session writing 10 GiB holds 10 GiB of resident memory and loses all
of it on crash.

ContextFS shares the flaw and shows how it happens. It *declares*
`WRITE_BUFFER_MAX_BYTES = 64 MiB` with an `over_cap()` predicate — and a grep for `over_cap`
across the whole tree returns the definition and **zero call sites**. The cap reads as
implemented and is dead code. `dofs` is the one that gets this right: a real 256 MiB per-file
cap enforced as `EFBIG` at four sites.

### The fix

Adopt the `dofs` pattern, which we are already wired for — `SfsError::OverBudget` maps to
`EFBIG` today.

1. **Bound staged bytes** with a configured cap, enforced at the point of staging, returning
   `OverBudget` (`EFBIG`) rather than growing without limit.
2. **Make the cap observable** so a caller can see pressure before hitting the wall, and so the
   mount can surface it.
3. **Test the enforcement, not the constant.** ContextFS's failure was a cap that existed as a
   number and never as a branch. The test must drive a workspace past the limit and assert the
   error.

Spill-to-disk is the more capable answer and none of the three implement it. It is deferred, not
rejected: chunks are content-addressed and immutable, so staging them into the store before
publication is the same operation the publication path already performs — the reason to wait is
that it widens the orphan window that GC exists to close, and that trade wants its own measured
decision rather than being smuggled in here.

### `fsync`

Two of the three do not deliver durability from `fsync`, and AgentFS's is worth reading as a
warning: it sets `PRAGMA synchronous = OFF` at open, then tries to compensate inside `fsync` with
`BEGIN; COMMIT;` on an *arbitrary pooled connection*. An empty transaction dirties no pages, so
there is nothing to flush; the pragma is per-connection, so it lands on the wrong handle; and the
sequence ends by setting `OFF` on a connection that may not have had it. `fsync(2)` there does
not do what its presence implies.

Our position must not be that. `fsync` makes staged data consistent and does not claim
durability, and it should say so — a mount option and documentation, not a no-op that returns
success and lets the caller assume a guarantee it did not get.

## What a real mount changed

The three questions above were answered before the adapter existed, and the answers held. One
thing only a real kernel could surface:

**`FUSE_ATOMIC_O_TRUNC` is a correctness requirement, not a tuning option.** Without it,
`open(O_TRUNC)` — what every `fs::write` to an existing file performs — arrives as `open` followed
by a separate `setattr(size=0)`. An adapter can only service that setattr by opening a *second*
handle on a path it already has open. Closing the second handle changes the file, which trips the
stale-handle protection in `Workspace::close`, and the caller's write is refused with no error.

The observable symptom was a file that read back empty after being overwritten. It reproduces in
`surrealfs-mount` on any platform, so it is pinned there rather than only in the Linux suite.

The tempting fix was to relax the stale-handle check, and that is wrong: the check is what stops a
handle opened before an MCP `fs_write` from clobbering it afterwards, and a test already pinned
that. The right fix was to stop the adapter from manufacturing the second handle at all. The
capability is requested in `init`, and a kernel that refuses it fails the mount rather than
proceeding into silent data loss.

## Verification

| | Where | What runs |
|---|---|---|
| `surrealfs-mount` | any platform | the semantics both adapters inherit |
| `surrealfs-fuse` | Linux, `docker/linux-test.Dockerfile` | a real kernel mount driven by POSIX syscalls |
| `surrealfs-sandbox` | both | Seatbelt and Landlock, each against spawned processes |

`scripts/linux-test.sh` runs the Linux half from a Darwin host. The minimal container privileges
are `--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined` — AppArmor is what
blocks `mount(2)`, not seccomp, and full `--privileged` is not required. Landlock needs none of
them.


## A bug the tests were shaped to miss

`open(O_TRUNC)` never truncated. `MountKernel::open` took a parameter named `create`, which mapped
to `OpenOptions::create()` — and that sets `truncate: false`. The FUSE adapter computed the flag
correctly from the open flags and the mount then discarded it.

What made it survive a test suite is worth recording. **Every test overwrote a file with *longer*
content**, which covers the old bytes and makes a missing truncate invisible. The bug only appears
when the replacement is shorter:

```
write "the original, rather long, content"
open(O_TRUNC); write "short"
read back -> "short" + "riginal, rather long, content"
```

The fix was to rename the parameter to `truncate`: the wrong name is what made the wrong mapping
look correct. Two regression tests now write shorter content — one in `surrealfs-mount`, which
runs anywhere, and one through a real FUSE mount.

This interacts with `FUSE_ATOMIC_O_TRUNC` above. Requesting that capability made `open` the *only*
path that truncates, so the two defects partly masked each other: the atomic-`O_TRUNC` work fixed
data loss on overwrite, and this one restored the truncation that work then depended on.

The lesson is not "write more tests". It is that test *inputs* need adversarial variety — longer
was the natural thing to write, and shorter was the case that discriminates.
