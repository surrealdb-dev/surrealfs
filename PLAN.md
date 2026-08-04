# SurrealFS master plan

## Objective

Build SurrealFS from scratch as a causal execution substrate for agents, using embedded SurrealDB
over SurrealKV as the canonical store. Test it against a backend-neutral reference model, hard
crash/invariant gates, and representative workloads. The initial product must make agent state inspectable,
forkable, attributable, and recoverable without maintaining multiple production engines or
duplicating filesystem semantics in every language SDK.

The reconstruction succeeds only if it produces user-visible capabilities that the existing AgentFS
architecture cannot provide reliably:

- atomic linkage between tool calls and state transitions;
- durable application commits and branch heads;
- named snapshots and cheap forks;
- filesystem- and artifact-aware diffs;
- provenance questions expressed over a native graph;
- recovery, evaluation, and policy decisions based on recorded execution history.

A storage-engine replacement without these outcomes is a failed reconstruction.

## Fixed decisions

1. **Canonical engine:** embedded SurrealDB backed by SurrealKV; no SQLite, AgentFS, raw SurrealKV,
   or other production adapter is planned.
2. **One production format:** no SurrealKV-only production adapter in the first release.
3. **One writer:** the Rust daemon owns the database directory and all state transitions.
4. **One semantic kernel:** language SDKs call the daemon; they do not reimplement filesystem rules.
5. **Immutable roots:** commits reference content-addressed namespace/inode/extent/KV roots;
   materialized heads are disposable projections.
6. **Graph is canonical:** provenance relations live in the same transactionally consistent store.
7. **Content addressing:** chunks and artifacts are named by hashes and treated as immutable.
8. **Portable escape hatch:** logical export/import is stable across physical engine upgrades.
9. **Restricted raw access:** raw reads are supported; writes to system records use domain commands.
10. **Honest replay:** SurrealFS promises captured-state restoration, not automatic determinism of
    external model or network behavior.
11. **Transactional publication:** tools write privately and publish through an explicit
    expected-head workspace commit; `close`/`fsync` never publish.
12. **Enforced attribution:** captured writes require daemon-issued workspace authority and verified
    process scope; trace context is correlation only.

## Workstreams

### A. Product and semantics

- Freeze the product contract and non-goals.
- Define filesystem behavior and supported deviations.
- Define commit, snapshot, branch, diff, merge, and replay semantics.
- Define execution spans and causal attribution.
- Select the first five product queries that must feel dramatically better than log inspection.

### B. Storage and data model

- Define schemafull SurrealDB tables and relation tables.
- Implement deterministic record IDs and schema versioning.
- Implement immutable persistent state roots plus optional measured/rebuildable head projections.
- Implement staged, content-addressed chunks backed by SurrealKV's value log.
- Implement atomic commit application and expected-head conflict detection.
- Implement logical export, import, integrity verification, and checkpoint policy.

### C. Runtime kernel

- Build `surrealfsd` as the only store owner.
- Centralize inode, dentry, extent, KV, and tool-span semantics in Rust.
- Implement streaming reads/writes and an open-handle table.
- Implement the Linux SDK/sandbox workspace first; add FUSE/NFS only after workflow demand.
- Provide backpressure, cancellation, quotas, and graceful shutdown.

### D. SDK and integrations

- Define a versioned local RPC protocol.
- Convert Rust, TypeScript, Python, and Go SDKs into clients.
- Preserve high-level AgentFS API compatibility where it does not violate new semantics.
- Integrate one representative agent framework end to end before broadening coverage.
- Add read-only graph query and event subscription APIs.

### E. Quality and operations

- Build a reference filesystem model and property tests.
- Run the private SurrealKV conformance suite in CI.
- Add process- and machine-crash fault injection at every durability boundary.
- Benchmark traced agent workloads, not only synthetic operations.
- Define upgrade, migration, backup, restore, and support procedures.
- Instrument latency, conflict rate, write amplification, cache usage, and recovery time.

## Phase sequence

### Phase 0 — design closure and SurrealDB/SurrealKV proof

Deliver:

- accepted immutable-root and transactional-workspace decisions;
- draft schema and IDs;
- the causal-commit protocol on SurrealDB/SurrealKV plus a pure reference model;
- crash/reopen, ancestry, export, lifecycle, complexity, and workload evidence;
- representative workload measurements and competitor context with semantic differences labeled;
- a representative imported AgentFS database;
- product-query fixtures;
- 5–10 discovery interviews and at least three prototype candidates;
- agreed performance and correctness budgets.

Exit with `GO`, `NARROW`, or `STOP` for SurrealFS on the fixed architecture. There is no engine or
build-vs-extend decision. Performance does not compensate for partial atomicity, missing roots, or
false attribution.

### Phase 1 — Linux causal-workspace vertical slice

Deliver one executable path:

1. Create repository and branch.
2. Begin a run/tool span and launch a capability-bound private workspace/process tree.
3. Stage directory/file, chunk, KV, and artifact changes invisible to committed readers.
4. Publish an immutable state root, provenance, branch head, and receipt atomically—or abort.
5. Close/reopen, create a constant-time pre-action fork, and compare state.
6. Answer `tool_call -> workspace -> caused -> commit -> produced -> artifact`.

Exit criteria:

- recovery returns the acknowledged state;
- a fault before commit exposes none of the new metadata;
- a retry with the same request ID creates no duplicates;
- a conflicting expected branch head is rejected;
- forged/missing capability, detached child, nested writer, and bypass attempts fail closed;
- the graph query returns only committed relationships.

### Phase 2 — design-partner recovery trial

Run the opinionated workflow on real failed coding-agent attempts: identify the first harmful
transition, restore/fork the exact pre-action state, run an alternative, compare files/KV/artifacts/
causality, and choose/export the result. Measure baseline and prototype recovery time, rerun cost,
integration effort, overhead, accuracy, repeat use, and willingness to continue.

Exit only when at least two of three partners repeat the workflow and a material outcome improves.
The proposed gate is at least 50% lower median recovery time or 30% lower rerun cost. Otherwise
`NARROW` or `STOP`; do not compensate with more filesystem features.

### Phase 3 — demand-gated filesystem correctness

Implement lookup, create, mkdir, read, write, truncate, rename, link, symlink, unlink, directory
listing, metadata, permissions, timestamps, xattrs, and open-handle semantics. Validate against the
reference model and relevant mount-level tests.

Begin only after Phase 2 `GO`. Exit when the partner-required subset passes semantic/crash tests and
storage choice remains invisible to the domain contract.

### Phase 4 — richer snapshots, diffs, and optional merge

Harden named snapshots and root-based forks, add correct ancestry/commit-range/content/provenance
diffs, and implement merge only if partner recovery requires it. Generation never proves ancestry;
head projections remain optional and root-keyed.

### Phase 5 — attribution and integration hardening

Extend the enforced workspace boundary across the integrations demanded by partners. Implement run
lifecycle, nested observational spans, explicit nested/concurrent writer policy, read/write sets,
artifacts, external observations, policy decisions, and evaluations.

### Phase 6 — SDK convergence and compatibility

Move all non-Rust SDKs to RPC. Add compatibility shims and deprecate direct database access. Ensure
that a conformance test produces identical logical results through every supported SDK.

### Phase 7 — migration and operational hardening

Implement AgentFS v0.4 import, backup/restore, logical export/import, upgrade checks, schema rollout,
integrity verification, quotas, observability, and disaster-recovery exercises.

### Phase 8 — workflow expansion

Expand beyond the proven recovery wedge only where evidence justifies it:

- explain why a path exists;
- rewind before a failed action;
- fork and compare agent outcomes;
- trace artifact consumers and derivations;
- evaluate a run against policy and historical baselines;
- identify the first causal divergence between two branches.

### Phase 9 — production decision

Compare the implementation against the previously agreed budgets. Proceed only if correctness,
performance, upgrade safety, and product value all pass. The database choice is not allowed to pass
on architectural enthusiasm alone.

## Cross-cutting acceptance gates

### Correctness

- No acknowledged commit is partially visible.
- A branch head always references an existing committed record.
- Every commit references a complete immutable state root.
- Optional materialized heads are disposable, root-keyed, and rebuildable.
- Every system-created relation references valid endpoints.
- Chunk references resolve to bytes whose hash matches their record ID.
- Link counts, directory membership, and open-unlinked behavior remain consistent.
- Imports label unknown provenance rather than fabricating it.
- Captured tool writes require verified workspace capability/process scope; missing context rejects.

### Durability

- Durable mode acknowledges only after SurrealKV's configured sync boundary.
- Reopen after injected failure recovers the last acknowledged commit.
- Staged unreferenced chunks are safe and reclaimable.
- Backup and logical export restore to the same logical root hash.

### Performance

- Budgets are defined separately for kernel operations, mount overhead, and end-to-end agent runs.
- p50, p95, and p99 are reported; averages alone are insufficient.
- Memory use, disk growth, write amplification, and reopen time are first-class metrics.
- Graph queries are tested at realistic run, artifact, and relation cardinalities.

### Product value

- At least three provenance/recovery workflows are meaningfully simpler than reconstructing logs.
- The first recovery workflow is repeatedly used by design partners before broad POSIX investment.
- Forking does not copy the complete state.
- A user can explain an artifact without manually correlating unrelated tables.
- The graph improves evaluation or recovery decisions, not merely dashboard aesthetics.

## Initial repository layout for implementation

```text
crates/
  surrealfs-types/       IDs, domain values, errors, serialization
  surrealfs-model/       Pure reference state machine and invariants
  surrealfs-store/       SurrealDB schema, queries, transactions, migrations
  surrealfs-content/     Chunking, hashing, staging, integrity, GC
  surrealfs-kernel/      Filesystem, KV, branch, snapshot, and span commands
  surrealfs-protocol/    Versioned RPC messages
  surrealfsd/            Daemon, store ownership, lifecycle, observability
  surrealfs-cli/         User commands and administrative operations
  surrealfs-fuse/        Linux mount adapter
  surrealfs-nfs/         macOS/userspace NFS adapter
sdk/
  typescript/
  python/
  go/
schema/
  migrations/
tests/
  model/
  crash/
  compatibility/
  workloads/
```

The implementation should not begin with every crate. Phase 1 can start with `types`, `store`,
`kernel`, and a minimal daemon, then split packages when stable boundaries become evident.

## Explicit kill criteria

Stop or change direction if any of the following remains true after focused optimization:

- filesystem metadata latency makes representative agent workloads materially worse than the
  agreed budget;
- SurrealKV cannot pass acknowledged-commit crash/recovery testing;
- schema or engine upgrades cannot be made repeatable without manual database surgery;
- the graph is used only for ornamental visualization;
- maintaining a private SurrealDB dependency consumes more engineering capacity than the product
  capabilities it enables;
- users reject the daemon ownership model required for consistent semantics;
- branch and history storage grows without an acceptable retention and compaction strategy.
- users do not repeatedly choose the recovery/fork workflow over Git, copy, worktree, or sandbox
  snapshots after a real trial;
- SurrealDB/SurrealKV cannot satisfy the required correctness, lifecycle, licensing, or workload
  contract within the agreed investment; stop or narrow rather than adding another adapter.

If a kill criterion is hit, preserve the domain model, logical export, and semantic kernel. Those
remain useful for a narrowed product and for users recovering their data from the stopped project.
