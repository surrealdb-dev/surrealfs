# Product contract

## Product definition

SurrealFS is a versioned execution environment for AI agents. It presents filesystem and KV state,
captures state transitions and agent actions, organizes those transitions into commits and branches,
and exposes a causal graph for explanation, recovery, comparison, evaluation, and governance.

## Primary users

- agent framework authors who need durable, inspectable state;
- developers debugging or comparing agent runs;
- platform teams operating agents in CI, sandboxes, or production workflows;
- evaluation teams requiring identical starting state and attributable outputs;
- security and compliance teams requiring evidence about agent actions;
- researchers studying agent behavior across controlled forks.

## Core promises

### Atomic state transition

When SurrealFS acknowledges a durable commit, its filesystem/KV mutations, immutable commit record,
causal attribution, relations, and branch-head update are visible together. The product never claims
that a tool caused a state change that was not committed.

### Restore captured state

A retained commit can be restored or mounted as the exact logical filesystem and KV state captured by
SurrealFS, subject to explicitly documented unsupported filesystem features.

### Trace provenance

For captured operations, SurrealFS can identify which run, span, tool call, process, or administrative
operation caused each committed mutation.

### Fork without full copy

A branch created from a retained commit initially shares immutable history and chunks. Creating the
branch does not duplicate the entire logical filesystem.

### Portable evidence

A logical export contains versioned domain records, relations, content hashes, and required chunk
bytes in a format independent of the SurrealKV directory layout.

## Qualified promises

### Replay

SurrealFS restores captured state and recorded inputs. It does not guarantee identical future behavior
unless every source of nondeterminism is pinned or replayed, including:

- model provider and model version;
- sampling parameters and provider-side changes;
- network responses and remote mutable state;
- wall clock, monotonic clock, and time zone;
- randomness and process scheduling;
- environment variables and secrets;
- host kernel and filesystem behavior;
- external services and human approvals.

The UI and API must distinguish `state_restored`, `inputs_replayed`, and `behavior_verified`.

### POSIX compatibility

SurrealFS aims for the subset required by supported agent workloads. Unsupported or approximated
features are reported explicitly. Compatibility is measured through conformance tests, not assumed
because a mount exists.

### Durability

Durability depends on the configured mode. The default production mode syncs every acknowledged
commit. Faster modes must expose their potential loss window in configuration and telemetry.

## Non-goals for the first production release

- a general replacement for APFS, ext4, XFS, or NTFS;
- multi-node distributed writes to one repository;
- arbitrary raw SurrealQL writes to system-owned records;
- byte-for-byte behavioral replay of external models and services;
- automatic semantic merging of every file type;
- executing untrusted code safely without a separate sandbox boundary;
- hiding all physical costs of unlimited history;
- supporting multiple canonical database formats;
- maintaining direct database semantics independently in every language SDK.

## Canonical operations

### Repository

- create/open/close;
- verify integrity;
- export/import;
- backup/restore;
- inspect format and schema versions.

### State

- filesystem lookup/read/write/mutate;
- KV get/set/delete/scan;
- begin and complete a causal span;
- apply an atomic commit request;
- read current or historical state.

### Version control

- name/delete snapshot reference;
- create branch from commit;
- list ancestry;
- diff commits or branches;
- merge with explicit conflict policy;
- mount a branch or commit read-only/read-write as permitted.

### Graph and evaluation

- explain path or artifact;
- traverse causes, reads, productions, derivations, and forks;
- compare runs;
- record evaluations and policy decisions;
- subscribe to committed run/branch changes.

## Consistency language

The product uses precise terms:

- **staged:** bytes or input have been uploaded but are not referenced by a commit;
- **committed:** a transaction made the record and its relations visible;
- **acknowledged:** the caller received a successful response;
- **durable:** the configured sync boundary completed before acknowledgement;
- **current:** referenced by the current branch head;
- **historical:** retained but not current;
- **reachable:** referenced from a retained repository root, branch, snapshot, or commit;
- **orphaned:** present physically but not reachable from a committed root;
- **verified:** hashes, references, and invariants passed integrity checks.

## Repository-level invariants

1. Repository IDs never change.
2. Branch names are unique within a repository.
3. A branch head is null only before its genesis commit.
4. Every non-genesis commit references valid parent commit(s).
5. Commit sequence is monotonic within a branch; commit identity does not depend on wall-clock order.
6. A snapshot references exactly one retained commit.
7. Current state records identify the commit that last modified them.
8. Historical state records are immutable.
9. A relation created by the kernel has valid endpoints in the same repository.
10. A referenced chunk's bytes hash to its content ID.
11. A successful mutation has a non-null cause, including explicit `system`, `import`, or `unknown`
    causes when finer attribution is unavailable.
12. System schema changes are applied through ordered, idempotent migrations.

## Service-level objectives to define before production

Numerical targets must be selected from measured workloads. At minimum define:

- availability and recovery objective for local daemon operation;
- maximum acceptable loss window for each durability mode;
- metadata-operation p95 and p99 budgets;
- streaming read/write throughput budgets;
- maximum reopen/recovery time at target repository size;
- fork and snapshot latency budgets;
- graph-query latency at target relation cardinality;
- backup, restore, and logical-export recovery objectives;
- maximum accepted integrity-check error rate: zero for committed references.

## Versioning policy

SurrealFS versions independently:

- RPC protocol;
- logical export format;
- domain schema;
- SurrealQL physical schema;
- daemon and SDK releases;
- minimum and maximum supported SurrealDB/SurrealKV versions.

An engine upgrade is not allowed to silently change product semantics. Compatibility tests compare
logical results before and after every supported upgrade path.

