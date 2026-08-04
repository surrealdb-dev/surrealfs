# Detailed Implementation Roadmap

## How to use this roadmap

This is a gated roadmap, not a promise to complete every phase. Each phase buys evidence needed to
justify the next investment. SurrealFS should stop or narrow if engine reliability, semantic cost,
or user value fails its gate.

Effort bands are rough **experienced engineer-weeks** for planning, not dates:

- `S`: 1-2 engineer-weeks;
- `M`: 3-6 engineer-weeks;
- `L`: 7-12 engineer-weeks;
- `XL`: 13+ engineer-weeks or a separate program.

Some work runs in parallel, but correctness-critical ownership stays explicit. A credible core
team is two senior Rust/storage engineers, one filesystem/runtime engineer, and one product/SDK
engineer, with part-time security, database, legal, and developer-experience review. A smaller
team can execute the same sequence with a longer calendar.

## Delivery principles

- Implement a thin vertical slice before broad POSIX or graph surface.
- Keep domain logic independent of SurrealDB types.
- Make crash/reopen and invariant checks part of each milestone, not a final hardening phase.
- Capture product telemetry only with permission and privacy controls.
- Never claim replay, merge, isolation, or durability beyond tested semantics.
- Preserve logical export from the first durable prototype.
- Treat schema and query changes like API changes.

## Phase 0 — proof package

### Goal

Decide whether SurrealDB + SurrealKV can support the required atomic semantic kernel and whether the
moat hypothesis is worth an implementation investment.

### Scope

| Work item | Effort | Deliverable |
|---|---:|---|
| Freeze product contracts and invariants | S | Reviewed versions of docs 01-07 |
| License/commercial review | S | Written decision for embedded distribution and hosted service |
| Pin engine/toolchain baseline | S | Minimal Cargo workspace and release manifest |
| Transaction spike | S | Atomic records + relations + conditional branch-head update |
| Chunk/value strategy spike | S | Measured options for in-DB chunks vs managed pack files |
| Crash harness | M | Kill points before/after engine commit and reopen verifier |
| Public SDK lifecycle spike | S | Drop/shutdown, lock-release, reopen, and error-observability report |
| Schema/query spike | S | Core schema applied to pinned SurrealDB revision |
| Workload capture | M | Sanitized representative AgentFS traces and target SLOs |
| Product discovery | M | 5-10 design partners ranked by painful workflow |

### Implementation backlog

- Create the Rust workspace described in `PLAN.md`.
- Define domain IDs, canonical serialization, mutation enum, commit receipt, and storage trait.
- Apply a minimal schema for repository, branch, commit, mutation, span/tool call, inode/dentry,
  file extent/chunk manifest, KV version, and idempotency receipt.
- Prove one transaction can validate expected head, insert an immutable commit and mutations,
  relate its author span, advance the branch, and store the request receipt.
- Run concurrent writers from separate client tasks and verify one wins cleanly.
- Terminate the daemon immediately before and after database commit; query the receipt on reopen.
- Benchmark 1k/10k mutations per commit and graph fan-out representative of traces.
- Verify public logical export/import, define a stopped-store physical recovery-copy procedure, and
  create a minimal engine-independent SurrealFS logical export.
- Test the exact pinned SurrealKV configuration, including sync and VLog behavior.
- Reproduce the SurrealKV 0.21.3 compaction/SIGKILL regression shape and retain it as a crash test.
- Drop all public SDK handles under load, verify route shutdown and directory lock release, then
  reopen repeatedly without importing hidden/internal constructors.
- Review SurrealDB/SurrealKV private/nightly API exposure and ensure only the adapter crate imports
  the public SDK.
- Interview design partners around: failed-run recovery, fork/compare, artifact provenance, and
  policy/audit. Ask for current workflow and cost, not feature enthusiasm.

### Exit criteria

- Atomicity and idempotency spike passes deterministic process-kill tests.
- Conditional head movement behaves correctly under at least 100 concurrent randomized campaigns.
- No required feature depends on an internal KVS API.
- Public SDK lifecycle either passes the required shutdown/reopen contract or has a supported
  upstream capability plan; crash correctness does not rely on graceful shutdown.
- Logical export/restore reproduces heads and state root for the spike.
- Initial latency, memory, and disk results are within a credible optimization distance of target.
- Legal approves the intended next-stage use or identifies an acceptable agreement/cost.
- At least three design partners rank one target workflow as a current high-cost problem and agree
  to evaluate a prototype using real or representative repositories.

### Stop/narrow conditions

- Engine loses acknowledged commits or produces partial semantic transactions.
- Licensing blocks the business model with no acceptable agreement.
- Point/range metadata access is structurally too expensive after schema/index profiling.
- No user has a painful workflow that requires causal filesystem state rather than ordinary tracing
  or version control.

### Phase artifact

An evidence report with raw benchmark/crash output and a written `GO`, `NARROW`, or `STOP` decision.

## Phase 1 — atomic vertical slice

### Goal

Deliver the smallest end-to-end SurrealFS experience: one repository, one branch, one run/tool
span, file and KV mutations, one atomic durable commit, reopen, and causal explanation.

### Core backlog

#### Domain and storage

- Implement strongly typed IDs and scoped repository types.
- Implement canonical mutation encoding and state-root hashing.
- Implement SurrealDB adapter with checked-in, numbered migrations.
- Implement repository creation and root commit.
- Implement branch head compare-and-swap and idempotency receipts.
- Implement staging for chunks plus abandoned-staging cleanup.
- Implement create/read/replace file, mkdir, list, stat, and KV get/put/delete.
- Implement run start/finish and tool span start/finish.
- Link span -> caused -> commit and commit -> produced -> artifact.

#### Daemon/protocol

- Start/stop lifecycle, exclusive directory lock, local socket permissions, health/readiness.
- Protocol negotiation, structured errors, deadlines, request IDs, and receipt lookup.
- Workspace open/stage/commit/abort with lease and quota.
- Read tree at explicit commit.
- Basic `Explain.Target` for file -> commit -> span -> run.

#### CLI

```text
surrealfs init
surrealfs daemon start|status
surrealfs run start|finish
surrealfs workspace open|write|kv-put|commit
surrealfs tree ls|cat
surrealfs explain <path>
surrealfs verify
surrealfs export|import
```

#### Quality

- Domain reference model and generated command sequences.
- Adapter transaction/reopen suite.
- Fault injection at all Phase 1 commit points.
- Golden logical export and restore.
- Basic auth via peer credentials and repository capabilities.
- Sanitized structured logging and request metrics.

### Exit criteria

From a clean install, a user can create a repository, record a tool span that writes a file and KV
checkpoint atomically, kill/restart the daemon, read the exact commit, and ask which run/tool caused
the file. Retrying the timed-out command returns the same receipt. A full logical export restores
the same state root in a second location.

### Effort

`L` for a narrow prototype; `XL` if shipped as supported cross-platform software.

## Phase 2 — filesystem correctness

### Goal

Turn the narrow tree store into a credible filesystem semantic kernel and one mounted integration.

### Backlog

- Complete inode/dentry model, link counts, directory invariants, and stable handles.
- Range writes, truncation, sparse holes, content-defined/fixed chunk strategy decision.
- Atomic rename matrix, hard links, symlinks, unlink-while-open, rename-while-open.
- Permissions, ownership mapping, timestamps, xattrs, umask, and explicit unsupported operations.
- Buffered mount workspaces and fsync/close/barrier mapping.
- Read cache keyed by immutable commit/inode/content identity.
- Concurrent handle and branch-head conflict behavior.
- Mount recovery after daemon restart and stale-handle errors.
- Directory pagination and large-directory performance.
- Per-repository quotas and inode/chunk accounting.
- Linux FUSE adapter first; macOS adapter after kernel semantics stabilize.
- Reference-model property coverage for all mutation types.
- Safe fstests subset and runtime filesystem tests.
- Crash matrix under mounted concurrent I/O.

### Exit criteria

- Documented POSIX subset passes its semantic and crash suite.
- No mounted operation can create state without a commit/mutation author boundary.
- Normal code-edit workload meets agreed p99 latency and memory SLO.
- One design partner can run a sandbox workload on a mounted SurrealFS repository without semantic
  surprises in the supported subset.

### Effort

`XL`. Filesystem correctness is likely the largest early engineering risk.

## Phase 3 — snapshots, forks, diffs, and merge

### Goal

Make immutable history useful to agent workflows rather than merely auditable.

### Backlog

- Named snapshots as commit references.
- Constant-time branch/fork creation from any retained commit.
- Commit ancestry, generation numbers, and lowest-common-ancestor queries.
- Tree/KV diff at summary and metadata levels.
- Text content diff with size/encoding bounds; binary summaries.
- Provenance diff: which spans/policies/evaluations explain changed state.
- Three-way merge engine for dentries, file content, metadata, and KV.
- Typed conflict objects and conflict-resolution workspaces.
- Merge commits with two parents and complete resolution mutations.
- Read-only snapshot mounts and branch checkout behavior.
- Retention roots and reachability-aware garbage collection.
- Branch-depth and ancestry performance tests.
- CLI/UI workflow for fork -> run variants -> compare -> select -> merge.

### Exit criteria

A user can fork a failed run's pre-error commit, execute two alternatives, compare file/KV/output
and provenance, select one, and merge without copying the entire repository. GC preserves all
retained histories and removes only proven unreachable staged/chunk content.

### Product checkpoint

Measure time saved and successful recovery rate versus users' existing copy-directory/git/manual
workflow. If fork/diff is not repeatedly used, narrow before expanding graph features.

### Effort

`L` to `XL`, depending on merge scope.

## Phase 4 — causal capture and artifacts

### Goal

Build the execution graph deeply enough to answer high-value causal questions reliably.

### Backlog

- Nested spans, normalized tool calls, ordered events, retries, and external-effect descriptors.
- Environment/input/output manifests with redaction and classifications.
- Artifact registration, content manifests, derivation, and file-at-commit linkage.
- Policy decision and approval records.
- Evaluation definition, subject, score, evidence, and evaluator identity/version.
- Bounded graph traversal service with cursors and read sequence.
- Durable event subscription catch-up plus live follow.
- Materialized product queries for run timeline, file explanation, artifact lineage, and evaluation
  comparison.
- Capture adapters for the two frameworks used most by design partners.
- Privacy configuration, per-field capture policy, and deletion/retention controls.
- Graph integrity tests and high-fan-out protection.
- External signed audit checkpoints for higher-assurance deployment.

### Exit criteria

- At least 95% of state-changing tool spans in supported integrations carry a correct commit edge;
  missing attribution is surfaced, not silently assigned.
- Explanation queries stay within SLO on target trace volume.
- Users can answer three chosen questions faster than with logs alone, such as “what caused this
  file?”, “which fork produced this evaluated artifact?”, and “what did this policy block?”
- Secret/redaction tests pass across database, logs, events, and exports.

### Effort

`L`.

## Phase 5 — SDK convergence and compatibility

### Goal

Make SurrealFS adoptable without binding clients to Rust or the database schema.

### Backlog

- Stabilize protocol v1 and capability negotiation.
- Rust reference SDK and conformance oracle.
- TypeScript and Python SDKs based on partner demand.
- Go SDK for infrastructure/sandbox integrations.
- Generated types wrapped in idiomatic domain APIs.
- Resumable streaming, backpressure, subscriptions, auth refresh, and structured retries.
- Framework adapters and instrumentation middleware.
- Analyst views and safe scoped raw SurrealQL service.
- Compatibility shims for current AgentFS callers where semantics can be preserved.
- API reference, examples, upgrade/deprecation policy, and integration test kit.

### Exit criteria

- All supported SDKs pass the same black-box corpus.
- Network disconnect after commit is handled without duplicate mutations.
- No SDK opens or depends on the database directory/schema.
- Two independent integrations adopt the API without changes to the semantic kernel.

### Effort

`L` across languages; prioritize based on actual users.

## Phase 6 — migration and operational hardening

### Goal

Safely move existing AgentFS repositories and operate SurrealFS through upgrades and failures.

### Backlog

- Versioned read-only AgentFS exporter and neutral migration bundle.
- Deterministic staging importer and synthetic genesis commit.
- Declared-loss and confidence model for legacy events.
- Source/target hash and state-root verifier.
- Shadow import, canary cutover, and rollback tooling.
- Stopped/quiesced physical recovery-copy procedure and logical export/restore automation; adopt a
  live physical snapshot only when the pinned engine exposes and passes a supported facility.
- Schema/engine upgrade orchestrator with cloned validation.
- Compaction/GC scheduling and observability.
- Quarantine/read-only recovery mode and integrity scanner.
- 72-hour and then multi-week soak campaigns.
- Operational runbooks: disk full, corruption, failed migration, key loss, subscription backlog,
  performance regression.
- SBOM, signed release metadata, dependency/license review.

### Exit criteria

- Production-sized migration has zero unexplained data difference.
- Upgrade and restore drills meet recovery objectives.
- Source rollback window and native-write divergence are understood by operators/users.
- Soak has no integrity failure or unbounded resource trend.
- On-call can diagnose every declared operational state from documented signals.

### Effort

`L`.

## Phase 7 — product workflows and moat validation

### Goal

Turn infrastructure into repeatable outcomes that generate switching costs and learning effects.

### Candidate workflows

1. **Failure recovery:** capture checkpoints, explain failure, fork before fault, retry safely.
2. **Variant tournament:** fork N approaches, evaluate consistently, compare provenance, merge best.
3. **Artifact chain of custody:** trace inputs/tool versions/policies/commits behind deliverables.
4. **Policy-gated agent execution:** enforce protected paths, approvals, cost and release criteria.
5. **Regression intelligence:** find execution patterns and state changes correlated with failures.

Choose at most two based on partner evidence.

### Backlog

- Opinionated CLI/UI workflow, not just primitive APIs.
- Saved explanations/comparisons with shareable scoped links/export.
- Evaluation harness and fork orchestration.
- Failure signatures and recovery templates.
- Permissioned aggregate metrics with opt-in, retention, export, and deletion controls.
- Integration marketplace or adapter kit for capture/enforcement points.
- Usage/value instrumentation: recovery time, successful variants, audit preparation time, avoided
  reruns, explanation query completion.
- Pricing and packaging experiments based on outcome value rather than stored bytes alone.

### Exit criteria

- Design partners use the selected workflow repeatedly without implementation-team assistance.
- At least one outcome metric materially improves versus baseline.
- Capture completeness and trust are high enough that users act on explanations.
- Users accept operational overhead and express meaningful unwillingness to lose accumulated
  history/workflows.
- A plausible willingness-to-pay and gross-margin model exists.

### Effort

`L`, but product iteration—not raw implementation—is the pacing factor.

## Phase 8 — production decision

### Goal

Decide whether to scale, narrow to a component, replace the engine, or stop.

### Production dossier

- reliability and crash-campaign record;
- SLO performance on supported hardware;
- backup/restore and upgrade evidence;
- security/threat-model review and penetration results;
- SurrealDB/SurrealKV license and support posture;
- unit economics at representative storage/trace volume;
- design-partner adoption, retention, workflow frequency, and outcome improvement;
- engine-change cost observed to date;
- privacy/governance approval for any learning flywheel;
- top residual risks with owners and contingencies.

### Decisions

#### Scale

Choose when correctness gates pass, workflows show repeated high value, retention and economics are
credible, and the engine has an acceptable production/support posture.

#### Narrow

Examples: ship causal capture + forkable workspace API without broad POSIX; focus only on regulated
artifact provenance; or make SurrealFS a local execution ledger rather than a general filesystem.

#### Replace storage adapter

Choose when product semantics validate but SurrealDB+SurrealKV fails reliability, latency,
licensing, or lifecycle gates. Preserve domain schema, RPC, logical export, and conformance suite.

#### Stop

Choose when causal filesystem semantics do not improve a costly user workflow or the required
engineering/operational complexity exceeds attainable value.

## Cross-phase dependency map

```text
domain invariants
  -> atomic commit vertical slice
      -> filesystem semantics -> snapshots/forks/diffs
      -> execution capture ----> product explanations
      -> logical export --------> migration/upgrades
      -> protocol --------------> SDKs/integrations

security + crash testing + observability span every arrow
```

## Suggested ownership

| Area | Primary owner | Required reviewers |
|---|---|---|
| Domain invariants/commit protocol | storage lead | filesystem, security |
| SurrealDB adapter/schema/upgrades | database lead | storage, operations |
| Filesystem kernel/mount | filesystem lead | storage, platform |
| Protocol/SDKs/integrations | product-platform lead | security, domain |
| Graph ontology/product queries | domain/product lead | database, users |
| Crash/benchmark infrastructure | reliability owner | all technical leads |
| Security/tenancy/keys | security owner | platform, operations |
| Migration | migration owner | source expert, storage |
| Moat/value validation | product owner | design partners, engineering |

No contributor owns “all correctness.” Cross-boundary changes require the reviewers shown.

## First 30 implementation issues

1. Create Rust workspace and CI skeleton.
2. Pin toolchain and embedded engine revision/features.
3. Define IDs and canonical encoding crate.
4. Define mutation and commit domain types.
5. Implement reference in-memory repository model.
6. Apply core schema migration to temporary SurrealKV database.
7. Implement release/schema manifest and startup compatibility check.
8. Implement exclusive database-directory lock.
9. Implement repository/root commit creation.
10. Implement branch expected-head transaction.
11. Implement request idempotency receipt.
12. Implement chunk hashing and staging state.
13. Implement inode/dentry create/read/list.
14. Implement file replacement and content read.
15. Implement KV get/put/delete.
16. Implement run and span lifecycle.
17. Implement atomic workspace commit across file + KV + span graph.
18. Implement state-root calculator and verifier.
19. Add named transaction fault points.
20. Add kill/reopen test controller.
21. Define protocol negotiation and error schema.
22. Expose local socket daemon health and repository APIs.
23. Expose workspace streaming API.
24. Build CLI for vertical slice.
25. Implement file explanation query.
26. Implement domain event sequence/subscription catch-up.
27. Implement minimal logical export.
28. Implement logical import into empty target.
29. Build representative workload benchmark harness.
30. Publish Phase 0/1 evidence report and re-evaluate go/no-go.

## Definition of done for every issue

- Domain behavior and non-goals are explicit.
- Tests cover success, conflict/error, retry, authorization, and reopen where relevant.
- New persistent fields have migration/export treatment.
- New queries are parameterized, scoped, indexed, and benchmarked if hot.
- Logs/metrics avoid sensitive payloads and have bounded cardinality.
- Documentation and protocol types change with behavior.
- Fault points exist for new transactional boundaries.
- No invariant failure is converted into an ordinary retry.
