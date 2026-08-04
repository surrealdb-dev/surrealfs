# ADR 0004: commits reference immutable state roots

- Status: accepted for the proof
- Date: 2026-08-04

## Context

SurrealFS promises exact historical reads, constant-time branch creation from a retained commit,
deterministic state identity, and safe comparison. Eager branch copies, layered base fallback, and
"latest version reachable through ancestry" have different correctness and performance behavior.
Leaving all three open makes the product contract untestable and makes generation numbers look like
ancestry when they are not.

## Decision

Every commit references one immutable, content-addressed `state_root`. The root identifies versioned
namespace, inode/metadata, extent/content, and KV roots. Updating state path-copies affected nodes and
shares unchanged nodes. A branch is a mutable name pointing to a commit; a snapshot is an immutable
name pointing to a commit. Creating either never copies the complete logical state.

Materialized branch-head records are optional disposable projections. They may accelerate reads only
when benchmarks justify them. They are never canonical identity, never required for a historical
read, and must be rebuildable and verifiable from the head commit's root.

The proof uses the smallest persistent tree that satisfies deterministic roots, historical reads,
forks, and diffs. It does not build a general storage engine or optimize every POSIX operation first.

## Consequences

- retained commits are self-identifying state boundaries;
- equality and subtree sharing can accelerate diff and verification;
- fork creation is independent of repository size;
- canonical encoding and node fanout become versioned format decisions;
- writes create multiple immutable nodes and need reachability-based GC;
- optional head projections require verification and rebuild tooling.

## Invariants

1. `commit.state_root` resolves to an immutable root whose digest matches canonical bytes.
2. Root children cover all committed filesystem and KV state and no workspace-staged state.
3. Generation is diagnostic/optimization metadata, never proof of ancestry.
4. Historical reads start from the selected commit root, not a branch-head table.
5. Deleting every materialized-head projection cannot destroy canonical state.
