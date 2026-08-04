# Agent execution graph

## Purpose

The execution graph turns stored state into an explainable product. It connects who acted, what they
observed, what they invoked, which commits changed state, which artifacts resulted, how branches
diverged, what policies applied, and how outcomes were evaluated.

The graph is canonical and transactionally linked to state. It is not reconstructed from timestamps.

## Core graph

```text
agent -> started -> run -> contains -> span
run/span -> invoked -> tool_call
span/tool_call -> owns -> workspace
workspace -> based_on -> commit
workspace -> published_as -> commit
span/tool_call -> caused -> commit
span/tool_call -> read/observed -> artifact/state
commit/span -> produced -> artifact
artifact -> derived_from -> artifact
branch/run -> forked_from -> commit/run
commit -> parent -> commit
evaluation -> evaluated -> run/commit/artifact
policy_decision -> governed -> span/tool_call
```

Where edge cardinality would be excessive, a set/summary record holds members or a digest and the graph
links to that record. Queries expose capture completeness.

## Run lifecycle

States:

```text
pending -> running -> success
                   -> error
                   -> cancelled
                   -> interrupted
```

Transitions are recorded as events. A run's final status does not delete or invalidate prior commits.
Restart/retry creates a new run or an explicit `retry_of` relation; it does not overwrite history.

Every run identifies:

- exact starting commit;
- branch and repository;
- agent configuration digest;
- environment manifest digest;
- model/provider metadata where applicable;
- root span;
- capture and redaction policy;
- selected outputs and evaluations.

## Span hierarchy

Span types include:

- workflow;
- agent reasoning/model invocation metadata;
- tool call;
- process execution;
- filesystem batch;
- external service request;
- human approval;
- policy evaluation;
- import/export/migration;
- merge/recovery operation.

Spans have explicit parent relationships and stable framework invocation IDs. Async or concurrent
work may share a parent and overlap in time; graph order uses causal edges and commit ancestry, not
timestamp sorting alone.

## Tool calls

A tool-call record stores:

- tool identity, version, and declared side-effect class;
- schema and parameter digest;
- encrypted/redacted payload reference as allowed;
- result/error digest and payload reference;
- framework IDs and retry lineage;
- process/network execution evidence;
- start/end times and status;
- capture level;
- produced commits and artifacts;
- declared observations and external side effects.

In the initial contract, one writable tool span owns one workspace and either publishes one commit on
success or aborts on error/cancel/timeout. An explicit checkpoint publishes that workspace and starts
a new workspace under a visible checkpoint/child-span boundary. A later tool error can coexist with
an earlier checkpoint commit, but uncheckpointed state is never silently retained.

## Causal attribution

Every commit has one direct cause record. Additional context flows through span ancestry. For
`CAPTURED` tool/process attribution, the direct cause is accepted only when publication presents the
daemon-issued workspace capability and verified process scope bound when the workspace opened.
W3C/OpenTelemetry trace identifiers correlate records but never authorize publication.

Cause types:

- tool/process span;
- model/workflow span;
- user/admin operation;
- merge;
- import;
- background retention/migration/repair;
- legacy unknown.

`unknown` is allowed only when source evidence cannot provide attribution, such as importing an
overwrite-only legacy database. It is explicit and measurable. Missing capability/context on a live
captured write is rejected rather than downgraded to unknown. System, migration, and maintenance work
uses a typed principal/cause and is never labeled captured tool execution.

The initial process contract also records:

- workspace/mount namespace and cgroup/process-scope identity;
- whether descendants inherited through the approved launcher;
- quiescence, timeout, kill, and abort evidence;
- nested/concurrent writer policy outcome;
- detected attempts to access the committed lower state or database directly.

## State observations

A read/observation edge should state what was actually captured:

- path as resolved at commit/view;
- inode or KV logical identity;
- content/state digest;
- branch/commit view;
- access range or semantic purpose where known;
- capture source: SDK, kernel, sandbox, declared, inferred;
- completeness and redaction flags.

Paths alone are insufficient because rename changes paths. Stable inode/artifact identities and view
commit make observations durable.

## Artifacts and derivation

Artifacts let product queries operate above raw files:

- a report may contain multiple files;
- a patch may be derived from source files and a model output;
- a binary derives from source, toolchain, and build inputs;
- a dataset may be external but referenced by immutable digest;
- one file version can participate in several semantic artifacts.

Artifact production records creator span/commit, manifest/content root, inputs, environment/tool
metadata, labels, and verification status. Derivation edges can include transformation name, version,
and evidence digest.

## Branch and fork graph

A fork records:

- source commit and optional source run;
- new branch and run;
- creator/cause;
- reason/experiment label;
- retained environment differences;
- time as metadata.

Comparisons use ancestry and state roots to identify the first divergent commit, then traverse causes
and artifacts around the divergence.

First-parent history follows the `first_parent` record link from the selected commit. Commit
generation is useful for diagnostics and indexes but never determines membership in a branch path.

## Evaluations

Evaluations are first-class evidence rather than mutable columns on runs. They reference:

- evaluator identity/version/configuration;
- subject and baseline;
- exact subject commit/artifact;
- assertions and scalar/vector scores;
- input evidence digests;
- result status and explanation;
- evaluator run/span if generated by an agent;
- policy and redaction context.

Multiple evaluations can disagree without overwriting one another. A selection policy determines the
current product view.

## Policy graph

Policy decisions capture:

- policy version;
- subject action;
- evidence considered;
- allow/deny/approval/constraint decision;
- decision source and human approval where applicable;
- enforcement attempt and result;
- exception scope and expiry.

This makes policy auditable and evaluable. A policy table alone is insufficient if enforcement cannot
be connected to the exact action and resulting commit.

## Product queries

### Explain a path

Given branch/commit and path:

1. Resolve dentry and inode version.
2. Find the commit that introduced the visible version.
3. Traverse `caused` to the span/tool.
4. Show input observations and parent span chain.
5. Show artifacts produced and subsequent consumers.

### Explain a failure

Given failed run/evaluation:

1. Find failed evaluation and subject.
2. Traverse run spans in causal/commit order.
3. Identify error spans and commits after last known-good snapshot/evaluation.
4. Compare with successful sibling runs/forks.
5. Surface first differing observation, tool result, mutation, or policy decision.

### Safe rewind

1. Select harmful span/commit.
2. Choose parent commit or last policy/evaluation checkpoint.
3. Create a new branch from that commit.
4. Preserve original evidence.
5. Record rewind/fork cause and optional remediation policy.

### Artifact impact

Traverse artifact consumers, derivative artifacts, evaluations, and branches. Use depth and scope
limits to avoid unbounded graph expansion.

### Agent comparison

Compare runs with identical starting commit and declared environment. Report state, actions,
artifacts, cost/latency metadata, policy outcomes, and evaluations.

## Query safety and boundedness

- Every product graph query requires repository/tenant scope.
- Default traversals have depth, result, time, and memory limits.
- Pagination order is deterministic and versioned.
- User-supplied raw queries run read-only with quotas.
- Sensitive payload fields are excluded or permission-filtered.
- Query plans and index usage are tested at target cardinalities.
- A graph edge does not bypass access control on its endpoint.

## Live updates

Live queries are useful for run timelines and branch activity. Durable catch-up must use committed
event/version cursors rather than assuming live delivery is a permanent ordered log.

The client subscription protocol:

1. reads current durable cursor;
2. subscribes to scoped notifications;
3. catches up committed events after its prior cursor;
4. deduplicates by event ID;
5. handles reconnect and retention expiry explicitly.

## Privacy and redaction

The graph should remain useful when raw content is unavailable. Prefer digests, typed summaries,
labels, and encrypted payload references. Redaction policy defines whether to retain:

- prompts and model outputs;
- tool parameters/results;
- source code and file bytes;
- environment variables;
- network request/response bodies;
- user and agent identity;
- secrets detected after capture.

Redaction is itself an auditable operation. Deleting a sensitive payload may retain non-sensitive
commit and relation evidence when policy permits.

## Graph quality metrics

- commits with known direct cause;
- spans with complete lifecycle;
- artifacts with producer and manifest;
- declared versus observed read coverage;
- relations rejected by invariant checks;
- legacy/unknown attribution rate;
- first-divergence query success rate;
- explain-path latency and completeness;
- live subscription recovery/deduplication rate;
- product use of explain, rewind, fork, compare, and impact queries.
