# Testing and Benchmark Strategy

## Purpose

SurrealFS correctness is not demonstrated by database unit tests alone. The system composes
filesystem semantics, optimistic concurrency, content addressing, graph provenance, crash
recovery, migrations, policy, and an embedded engine. The test program must verify the product
contract at every boundary and must actively search for histories that violate invariants.

## Test pyramid

```text
production canaries and restore drills
long-running soak, compaction, crash campaigns
cross-process protocol and mounted filesystem suites
SurrealDB adapter integration and migration tests
domain state-machine/property tests
pure unit tests and static checks
```

The domain model is tested independently of SurrealDB using an executable reference model. The
same black-box conformance suite then runs against the in-memory test store and embedded
SurrealDB+SurrealKV.

## Non-negotiable invariants

After every generated command and every simulated recovery:

1. A branch head references one existing commit in the same repository.
2. Every non-root commit has valid parent edges; the commit graph is acyclic.
3. A successful receipt corresponds to exactly one canonical command outcome.
4. All mutations in a commit are visible together or not visible.
5. Materializing a commit yields its recorded state root.
6. Each reachable dentry points to an inode in the same repository.
7. Every non-root reachable inode has the expected link/reachability accounting.
8. File extents are ordered, non-overlapping, and resolve to valid immutable chunks or holes.
9. Chunk bytes match their versioned content IDs.
10. A KV version belongs to exactly one commit and reads follow commit ancestry/materialization
    rules.
11. A `caused` edge points from the author span/tool call to the commit it actually submitted.
12. Relation endpoints cannot cross tenant/repository boundaries accidentally.
13. Visible commits never reference uncommitted staged data.
14. A rejected or conflicted command never advances a branch.
15. Garbage collection never deletes data reachable from a retained commit, snapshot, artifact, or
    active workspace.

## Unit tests

Pure tests cover:

- ID canonicalization and deterministic derivation;
- byte-path parsing, joins, root boundaries, and normalization modes;
- extent splice/truncate/hole algorithms;
- chunking and state-root algorithms with golden vectors;
- mutation canonical ordering and serialization;
- expected-head and idempotency state machines;
- diff and three-way merge rules;
- authorization/policy input canonicalization;
- error classification and retry advice;
- export framing/checksums;
- quota arithmetic and overflow boundaries.

Golden encodings are versioned. Any intentional change requires a migration/export-format decision,
not a casual snapshot update.

## Model-based and property testing

A small reference repository model holds complete immutable trees and KV maps in memory. A
generator produces commands such as create, write range, truncate, rename, link, unlink, symlink,
KV put/delete, fork, commit, conflict, merge, snapshot, and GC.

For each sequence:

1. apply valid commands to the reference model and system under test;
2. compare success/error category and receipt;
3. compare heads, tree walks, reads, KV scans, diffs, and state roots;
4. reopen the database at generated points;
5. shrink failures to a minimal reproducible history.

Generators emphasize boundary cases:

- empty and maximum-length names;
- raw non-UTF-8 names on Unix;
- sparse files and overlapping range writes;
- rename over existing entries and directory cycles;
- hard links followed by unlink/rename;
- simultaneous workspaces with the same base;
- repeated and mismatched idempotency keys;
- branch diamonds and deep ancestry;
- high graph fan-out and cycles among permitted analytical edges;
- quota boundaries and integer overflow.

State-machine concurrency testing uses randomized schedules and a linearizability checker for
branch-head compare-and-swap, idempotency receipts, and file/KV atomic commits.

## Storage-adapter tests

Each adapter must pass a shared contract:

- atomic commit and rollback;
- immutable state-node/root creation, historical read, structural sharing, and root verification;
- workspace isolation, explicit publish/abort, and no visibility on `close`/`fsync`;
- capability/process-scope enforcement and missing-context rejection;
- conditional head movement;
- deterministic receipt replay;
- snapshot-consistent reads;
- ordered scans and pagination;
- reopen durability;
- large value/chunk metadata handling;
- cancellation and timeout behavior;
- migration version detection;
- logical export/import boundary behavior;
- public SDK drop/shutdown, store-lock release, and reopen behavior;
- stopped-store physical recovery-copy verification.
- first-parent pagination that excludes unrelated same-generation branch commits.

Evidence revalidated against local SurrealDB `v3.3.0-nightly` source commit `e68539867` with
SurrealKV `0.21.3`:

- shared SurrealKV KVS target: 73 passed, 0 failed, 8 ignored;
- SurrealKV adapter unit tests: 3 passed, 0 failed;
- targeted public-SDK tests passed for client transactions, session transaction isolation, live
  select, logical export/import, versioned select, and versioned export/import data types.

This is upstream dependency evidence only. The ignored shared-harness cases included low-level
versioned behavior under its default non-versioned configuration and backend/harness-specific cases;
separate targeted public-SDK versioned tests passed. None of these results substitute for SurrealFS
domain, workload, lifecycle, or fault tests. See the [current source audit](15-current-surrealdb-audit.md)
for exact commands and limitations.

SurrealKV `0.21.3` carries upstream fixes for compaction fsync after a reported SIGKILL/zero-byte
SSTable failure and for memtable rotation. Add the reported failure sequence to permanent regression
coverage rather than treating the dependency update as sufficient proof.

## Transaction fault injection

The commit protocol defines named fault points:

```text
after_request_receipt_lookup
after_chunk_stage
after_begin_transaction
after_head_read
after_workspace_capability_validation
after_process_quiescence_check
after_each_mutation
after_each_state_node
after_state_root
after_commit_record
after_graph_edges
after_branch_update
before_engine_commit
after_engine_commit_before_receipt_reply
after_receipt_reply
while_grouped_wal_flush_is_pending
during_compaction_before_manifest_sync
during_sdk_drop_and_engine_shutdown
```

Tests terminate the daemon or inject an error at each point across representative commands, then
reopen and assert invariants. Outcomes allowed:

- command absent and same request can retry;
- command fully present and same request returns `REPLAYED`;
- never partial state or an unexplained branch head.

Fault campaigns vary sync/group-commit settings, file sizes, concurrent readers, compaction load,
and disk-full timing. Disk I/O error shims cover short writes, failed sync, corrupted blocks where
the engine supports detection, permission changes, and exhausted space.

## Crash and durability testing

Process kills are necessary but not sufficient. The matrix includes:

| Failure | Method | Required result |
|---|---|---|
| Client disappears | kill SDK process mid-stream/mid-response | workspace expires; committed receipt remains discoverable |
| Tool descendant outlives parent | keep cgroup child alive past tool exit/deadline | publish waits, then policy kills/aborts; no staged state visible |
| Forged/missing capability | alter trace/span/capability/process scope | fail closed; no downgrade to captured/unknown |
| Daemon abort/SIGKILL | randomized kill loop | last acknowledged durable commits survive; no partial commit |
| Host restart | VM/container/host test where feasible | identical durability contract after restart |
| Disk full | quota/loopback fault environment | clear failure, no false success, recovery after space restored |
| Read-only/permission loss | faulted database directory | fail closed; no data replacement elsewhere |
| Corruption | controlled block/pack alteration | detected by engine, chunk hash, state root, or verifier |
| Clock discontinuity | jump forward/back | IDs/order do not rely on wall-clock monotonicity |

A campaign records seed, binary and engine revisions, filesystem, mount options, hardware/storage
class, configuration, fault point, and last receipts so every failure is reproducible.

## Filesystem semantic tests

The direct tree API and mount adapter share a semantic corpus:

- create/open/close/read/write/truncate/fsync;
- mkdir/rmdir/readdir and stable pagination;
- rename permutations, including atomic replacement;
- hard links and link counts;
- symbolic links, dangling targets, loops, and no-follow behavior;
- permissions, ownership mapping, times, xattrs, and unsupported operations;
- sparse files, holes, large offsets, and concurrent handles;
- unlink-while-open and rename-while-open;
- crash between buffered writes and commit barriers;
- snapshot/branch views and read-only mounts.

On Linux, run a documented safe subset of fstests/xfstests plus language/runtime file suites after
the adapter is stable. Every skipped case names the missing semantic or deliberate non-goal.
macOS has an equivalent platform suite for the chosen FUSE implementation. Passing generic tests
does not waive SurrealFS-specific provenance assertions: each operation's resulting commit and
mutation set are checked.

## Graph and provenance tests

Fixtures encode known execution DAGs with branches, retries, artifacts, policy decisions, and
evaluations. Tests assert:

- direct and transitive cause queries;
- file-at-commit resolution before graph traversal;
- artifact lineage and derivation direction;
- fork ancestry and lowest common ancestors;
- no duplicate edge on request replay;
- imported versus captured confidence;
- traversal bounds/cursors;
- field-level redaction;
- graph results remain correct after schema migration and GC;
- materialized product views match canonical records.

Mutation testing should delete or reverse graph edges in a fixture to prove the suite detects the
error.

## API and SDK conformance

A language-neutral corpus specifies request bytes/JSON, expected response, error code, and state
effects. Rust, TypeScript, Python, and Go SDKs run it against the same daemon build.

Coverage includes negotiation, unknown fields, deadlines, reconnect, idempotent retry,
pagination, stream cancellation, backpressure, authentication expiry, subscriptions and cursor
gaps, path bytes, large integers, and structured errors. Network proxy tests inject duplication,
delay, reordering across independent calls, truncation, and disconnect after server commit.

## Security testing

- Cross-tenant/repository matrix for every endpoint, relation, query, subscription, export, and
  error response.
- Fuzz protocol decoders, path walker, archive/import parser, SurrealQL bindings, mutation decoder,
  and export reader.
- Attempt statement and identifier injection through every string/byte input.
- High-fan-out graph, tiny-file, deep-path, deep-ancestry, compression bomb, and slow-stream DoS.
- Verify secrets never enter default logs, traces, metrics labels, error bodies, or previews.
- Test token expiry, revocation, wrong audience, key rotation, peer-credential mismatch, and replay.
- Audit recovery-mode entry and ensure agents cannot enable it.
- Restore encrypted backups with correct keys and fail safely with wrong/revoked keys.

## Migration and upgrade tests

For every supported source and prior SurrealFS schema:

- golden fixtures including empty, typical, maximum-size, ambiguous, and corrupt stores;
- source remains byte-for-byte unchanged;
- deterministic export and idempotent import;
- exact path/content/KV comparison;
- declared-loss report comparison;
- interruption at every migration phase;
- disk-full and restart recovery;
- old binary export -> new binary import;
- physical clone upgrade and state-root comparison;
- unsupported downgrade refusal;
- rollback using verified source/copy.

## Benchmark workloads

Microbenchmarks locate costs; end-to-end workloads decide fitness.

### Metadata-heavy agent workspace

100k files, median 2 KiB, nested dependency directories, repeated stat/list/open, 1k mutations per
commit. Measures point lookup, directory scan, transaction size, index amplification, and memory.

### Code-edit loop

Checkout base, read 200 files, write range/replace 5-30 files, create/delete paths, update 20 KV
keys, commit from one tool span, diff, then fork. Measures commit latency, chunk reuse, diff latency,
and bytes written.

The Phase 0 proof form is fixed and smaller: five file mutations including rename/delete, 20 KV
updates, one artifact/span, publish, reopen, historical read, first-parent pagination, fork, diff, and
explain. The SurrealDB/SurrealKV result must match the pure reference model and durability contract.

### Artifact-heavy run

Stream 1-10 GiB artifacts with a mix of 4 KiB to 256 MiB objects while metadata commits continue.
Measures throughput, tail latency interference, value-log/pack behavior, memory, and recovery.

### Trace-heavy run

10k spans, 100k events, 1k commits, artifacts and policies, concurrent dashboard subscription.
Measures capture overhead, graph indexes, event pagination, and live/catch-up delivery.

### Branch/evaluation workload

Fork 100 variants from a shared base, make small divergent changes, evaluate all, query best result,
and merge one. Measures storage sharing, ancestry, graph query, and GC.

### Long-lived repository

Millions of commits/events with realistic retention, compaction, backup, verify, and reopen.
Measures history growth, write/read amplification, maintenance interference, and recovery time.

## Comparative baselines

Phase 0 measures:

1. the SurrealFS pure reference/in-memory model as a semantic and overhead floor;
2. the canonical embedded SurrealDB + SurrealKV implementation;
3. raw SurrealDB queries used by the adapter to isolate semantic-layer overhead;
4. the existing AgentFS product only where an end-to-end workload is genuinely comparable.

No SQLite/AgentFS adapter or AgentFS extension is implemented. AgentFS results are competitor context,
not an engine-selection benchmark; capability and durability differences are labeled explicitly.

The comparison must normalize durability. An asynchronous/non-fsync baseline cannot be presented
as faster than a durable commit without labeling the mismatch. Feature differences such as graph
capture and immutable history are reported separately from throughput.

Correctness is pass/fail before performance analysis. If the fixed stack loses acknowledged state,
partially publishes, misattributes a write, reconstructs a different root, or returns sibling-branch
commits, SurrealFS stops or narrows; latency cannot compensate. After correctness, report owned
code/complexity, migration/query surface, lifecycle, and operator burden alongside performance.

## Metrics

For every workload capture:

- operations/sec and p50/p95/p99/p99.9 latency;
- commit size and mutations per transaction;
- CPU time, resident/peak memory, allocations where practical;
- logical bytes, physical bytes, write amplification, value-log and index growth;
- chunk deduplication ratio and unreachable staging bytes;
- reopen, verification, export, import, backup, restore, compaction, and GC time;
- query rows scanned/returned and graph nodes/edges traversed;
- subscription lag and duplicate deliveries;
- error/conflict/retry rate;
- hardware, OS, filesystem, engine revision, and full configuration.

Results include confidence intervals and raw machine-readable output. Warm/cold cache and foreground
maintenance state are separate runs. A performance change needs a reproducible benchmark, not a
single laptop number.

## Initial acceptance gates

Concrete thresholds are set in Phase 0 using observed product workloads. Until then, these
relative gates prevent vague success:

- zero invariant or acknowledged-durability failures in the full deterministic crash matrix;
- zero staged-state visibility before publish and zero captured writes accepted with invalid/missing
  workspace authority;
- zero ancestry-membership errors in branched first-parent fixtures;
- retained roots reconstruct exact file/KV state and fork without full-state copy;
- zero unexplained differences in migration and export/import verification;
- p99 metadata read and normal commit latency fit the user-interaction SLO on supported hardware;
- enabling provenance adds no more than the explicitly approved latency and storage budget;
- resident memory stays within the daemon deployment budget at the 95th-percentile repository;
- 10x target history does not show unbounded latency or disk-amplification slope;
- backup/restore and verification finish inside recovery objectives;
- compaction/GC cannot stall commits beyond the approved p99.9 budget;
- SurrealDB upgrade passes old/new logical equality and a production-sized canary soak;
- no critical/high unresolved security finding for the deployment model.
- Phase 0 stack report contains reference-model, crash, lifecycle, query-plan, and workload evidence.
- Phase 2 design-partner trial demonstrates repeated recovery use and a material outcome improvement
  before broad filesystem compatibility work begins.

## Continuous validation

Pull requests run units, model sequences, adapter integration, schema checks, migration fixtures,
and a short crash sample. Nightly runs expand seeds, platforms, mounted tests, fuzz time, and
benchmarks. Weekly runs long crash/soak/compaction campaigns. Release candidates run the complete
matrix, upgrade/restore drills, and signed benchmark report.

Any production integrity anomaly creates a minimized fixture and permanent regression test before
the incident is considered closed.
