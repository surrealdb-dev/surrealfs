# API and SDK Design

## Objective

The public API must expose SurrealFS concepts rather than SurrealDB tables. Clients create runs,
open spans, mutate repository state, fork commits, query provenance, and export data. They do not
need to know whether the store uses documents, relation tables, or key ranges.

This boundary is the product's compatibility layer and the escape hatch from the selected engine.

## Process boundary

All durable access goes through `surrealfsd`. The default local transport is a Unix domain socket
on Unix-like systems and an equivalent restricted local IPC mechanism on other platforms. A
remote TLS transport can be added later without changing domain messages.

```text
FUSE / CLI / Rust SDK / TypeScript SDK / Python SDK / Go SDK
                             |
                 versioned RPC protocol
                             |
                 auth + admission + policy
                             |
                    domain command service
                             |
                  SurrealDB storage adapter
```

The protocol should use Protobuf or an equivalently strict interface definition with:

- stable numeric field identifiers;
- explicit optionality;
- unknown-field preservation where supported;
- request and response size limits;
- protocol and capability negotiation;
- generated SDK types;
- canonical JSON mapping for debugging and automation.

The wire technology is replaceable; the semantic definitions and error behavior are not.

## Version negotiation

The first call is `System.Negotiate`:

```text
request:
  client_protocol_min
  client_protocol_max
  client_name
  client_version
  requested_capabilities[]

response:
  selected_protocol
  server_version
  schema_version
  export_version
  capabilities[]
  limits
  deprecation_notices[]
```

The server refuses a client only when no compatible protocol exists or a required capability is
missing. Additive fields remain compatible. Removing or changing meaning requires a new major
protocol. Server behavior never silently depends on SDK version.

## Command/query separation

Commands change domain state; queries only observe it. Every command passes through validation,
authorization, policy evaluation, idempotency, and audit capture. Queries receive a consistent
read boundary and explicit pagination.

Command examples:

- `Repository.Create`, `Repository.Archive`
- `Branch.Create`, `Branch.Move`, `Branch.Merge`
- `Workspace.Open`, `Workspace.Launch`, `Workspace.Publish`, `Workspace.Abort`
- `Run.Start`, `Run.Finish`, `Run.Cancel`
- `Span.Start`, `Span.RecordInput`, `Span.Commit`, `Span.Finish`
- `Artifact.Register`, `Evaluation.Record`, `Policy.RecordDecision`
- `Admin.Import`, `Admin.Migrate`

Query examples:

- `Repository.Get`, `Repository.List`
- `Tree.Stat`, `Tree.List`, `Tree.Read`, `Tree.Diff`
- `KV.Get`, `KV.Scan`, `KV.History`
- `Commit.Get`, `Commit.Log`, `Commit.Materialize`
- `Run.Get`, `Run.List`, `Run.Timeline`
- `Explain.Target`, `Graph.Traverse`
- `Artifact.Lineage`, `Evaluation.Compare`
- `Admin.Health`, `Admin.Verify`, `Admin.Export`

## Core request envelope

Every mutating request contains:

```text
request_id       caller-generated UUID/ULID, stable across retries
repository_id    mandatory scope for repository data
principal        derived from authenticated session, never trusted from payload
deadline         absolute or relative execution deadline
expected_head    required for head-moving mutations unless explicitly force-authorized
run_id           optional execution context
  span_id          causal context for non-workspace events; bound at Workspace.Open for writes
  trace_context    correlation only; never publication authority
command          typed body
```

The receipt contains:

```text
request_id
outcome            APPLIED | REPLAYED | CONFLICT | REJECTED
commit_id          when a commit exists
previous_head
new_head
domain_sequence
durability_profile
policy_decision_ids[]
warnings[]
```

Repeating a request ID with byte-for-byte equivalent canonical input returns the stored receipt.
Repeating it with different input returns `IDEMPOTENCY_KEY_REUSED`; the server never guesses.

## Workspace transaction API

Filesystem callbacks are too fine-grained to equate every write syscall with a durable semantic
commit. SDK users operate through a workspace:

```rust
let ws = client.workspace().open(OpenWorkspace {
    repository,
    branch: "main",
    base: ExpectedHead::Current,
    author_span: span_id,
    process_policy: ProcessPolicy::NoDetachedDescendants,
}).await?;

let tool = ws.launch(command).await?; // descendants inherit the scoped workspace
tool.wait_for_quiescence().await?;

let receipt = ws.publish(PublishOptions {
    request_id,
    message: "produce final report",
    durability: Durability::Durable,
}).await?;
```

The SDK handle carries an opaque, short-lived workspace capability. Its hash/identity is bound to the
repository, base commit, principal, author span, process scope, permissions, and expiry; the raw
secret is never persisted or accepted from a trace header. File, KV, and artifact writes made by the
launched process tree use that workspace automatically.

The server may stage large chunks before the final transaction, but staged content is invisible to
committed readers until publish succeeds. Workspaces have leases, byte/count limits, explicit abort,
and garbage collection after expiration. Publish rejects a missing/invalid capability, live forbidden
descendants, nested writable workspace, expired lease, or stale expected head.

`close` and `fsync` affect workspace-local handles and staging only; neither publishes. The initial
write surface is the direct SDK/sandbox launcher. A later mount adapter uses the same internal command
API and must expose an explicit publish/checkpoint control rather than invent a syscall boundary.

## File and tree API

All path APIs accept raw path bytes on Unix-capable transports or an explicitly encoded portable
path type. They never normalize away meaningful bytes. Portable SDK helpers may offer UTF-8 paths
but must identify that narrower contract.

Essential operations:

```text
Stat(at_commit, path, follow_symlinks)
List(at_commit, directory, cursor, page_size)
Read(at_commit, path, offset, length)
ReadStream(at_commit, path, ranges[])
Diff(from_commit, to_commit, prefix?, detail_level)
Blame(at_commit, path, byte_or_line_range)
ResolvePath(at_commit, path)
```

Reads default to an explicit commit returned by branch resolution at request start. Paginated
directory traversal holds that read boundary in its signed cursor so concurrent head movement
cannot duplicate or omit entries.

## Agent KV API

KV keys are opaque bytes within a repository and namespace. Values have media type, optional
application schema, author span, and commit identity.

```text
KV.Get(at_commit, namespace, key)
KV.Scan(at_commit, namespace, prefix, cursor, page_size)
KV.History(namespace, key, before_commit, limit)
Workspace.KVPut(namespace, key, value, precondition)
Workspace.KVDelete(namespace, key, precondition)
```

Preconditions support absent, value-hash equals, and last-commit equals. KV mutations commit with
filesystem mutations under one expected branch head.

## Execution capture API

A run is the unit users compare and evaluate. A span is a causally nested operation. Tool calls
are typed spans with normalized tool metadata.

```text
Run.Start(agent, objective, parent_run?, fork_commit?, environment_manifest)
Span.Start(run, parent_span?, kind, name, input_manifest)
Span.AppendEvent(span, sequence, kind, payload_ref)
Workspace.Open(span, base_commit, process_policy)
Workspace.Publish(request_id, expected_head)
Workspace.Abort(reason)
Span.Finish(status, output_manifest, usage, external_effects)
Run.Finish(status, result_artifacts, evaluation_requests)
```

Inputs and outputs larger than configured limits are stored as artifacts; records contain hashes,
media types, sizes, and redacted previews. Each event carries a caller sequence and request ID so
retries cannot duplicate timelines.

The SDK supplies framework adapters, but the protocol remains framework-neutral.

## Provenance and explanation API

`Explain.Target` accepts a file path at a commit, artifact, commit, KV key, or evaluation and
returns a bounded typed subgraph:

```text
target
nodes[]             stable IDs, types, summaries, timestamps
edges[]             type, direction, evidence
truncated           true when limits stopped traversal
continuation        signed cursor for more traversal
confidence          CAPTURED | IMPORTED | INFERRED
read_at_sequence
```

Callers select allowed edge kinds, directions, maximum depth, node limit, time range, and whether
payload previews are included. The server enforces hard ceilings to prevent graph-amplification
attacks.

The confidence field is essential. A migrated file may be linked to a synthetic import commit but
must not be presented as though its original tool call was observed.

## Diff and merge API

Diff levels are explicit:

- `SUMMARY`: counts and changed paths;
- `METADATA`: inode/dentry/KV mutations and hashes;
- `CONTENT`: bounded textual/binary diff where supported;
- `PROVENANCE`: mutations plus author spans, policies, artifacts, and evaluations.

Merge accepts a base, ours, theirs, strategy, and expected destination head. It returns either an
atomic merge commit or a typed conflict set. Conflict resolution creates a new workspace; it never
edits existing commits.

## Streaming and backpressure

Large files, exports, imports, timelines, and subscriptions use bounded streams. Each stream has:

- content hash and declared length where known;
- fixed maximum frame size;
- application-level flow control;
- deadline and idle timeout;
- cancellation;
- resume token for export/import where safe;
- end-to-end checksum verification.

Servers reject declared or accumulated limits before memory exhaustion. Clients must not assume an
entire artifact can be buffered.

## Subscriptions

`Events.Subscribe` is cursor-based, not a bare live query:

```text
repository_id
after_domain_sequence
filters { run, branch, event_kinds }
include_payloads
heartbeat_interval
```

Delivery is at least once. Event IDs are stable; consumers de-duplicate and checkpoint sequence.
The server catches up from durable event records before following new commits. A retention gap
returns `CURSOR_EXPIRED` with the earliest available sequence and an export/query recovery path.

## Error model

Errors have a stable code, human message, retry class, structured details, and correlation ID.

| Code | Meaning | Retry behavior |
|---|---|---|
| `HEAD_CONFLICT` | Branch moved from expected head | Re-read, merge/rebase, submit a new request ID |
| `IDEMPOTENCY_KEY_REUSED` | Same request ID, different command | Caller bug; do not retry |
| `NOT_FOUND` | Scoped object absent or hidden | Do not retry without state change |
| `PERMISSION_DENIED` | Principal lacks capability | Do not retry unchanged |
| `POLICY_REJECTED` | Captured policy denied operation | Change operation or obtain approval |
| `RESOURCE_EXHAUSTED` | Quota or bounded limit exceeded | Retry only after reducing work or quota change |
| `UNAVAILABLE` | Daemon/engine temporarily unavailable | Exponential backoff with same request ID |
| `DEADLINE_EXCEEDED` | Outcome may be unknown | Query receipt with same request ID before retry |
| `INTEGRITY_FAILURE` | Stored state violates invariant | Stop writes; operator intervention |
| `UPGRADE_REQUIRED` | Protocol/schema compatibility absent | Upgrade client or server |

HTTP or gRPC status codes are secondary mappings. SDK logic keys on domain code and retry class.

## Authentication and authorization surface

Local socket peer credentials authenticate the operating-system principal where supported. Remote
transports require mutually authenticated TLS or short-lived signed tokens. The server derives a
principal and evaluates capabilities at repository, branch, operation, and data-class scopes.

The API does not accept database credentials from ordinary clients and does not proxy arbitrary
SurrealDB authentication.

## Raw SurrealQL interface

Raw SurrealQL is valuable for inspection and research, but unrestricted writes would bypass the
semantic kernel and destroy the moat. The interface is divided into three modes:

### Safe query mode

- read-only;
- server-selected namespace/database;
- mandatory repository scope injected or validated;
- statement allowlist (`SELECT`, bounded graph traversal, safe `LET`);
- time, result-row, traversal, memory, and response-byte limits;
- no functions or clauses with mutation/side effects;
- queries logged by hash and principal;
- output passes field-level redaction.

### Analyst views

Stable, documented read models such as `v_run_summary`, `v_commit_provenance`, and
`v_artifact_lineage` shield users from physical schema churn. These views or service queries are
the preferred escape hatch.

### Unsafe administrator mode

Direct write access exists only as an offline, explicitly enabled recovery tool. It requires the
daemon to enter maintenance mode, a verified backup, local operator authorization, a recorded
reason, and a post-write invariant scan. It is never available to agent code or remote tenants.

The guarantee is clear: data written outside domain commands is unsupported unless produced by an
official migration or recovery procedure.

## SDK tiers

1. **Rust reference SDK:** full API, streaming, mount integration helpers, and conformance oracle.
2. **TypeScript SDK:** agent-framework and Node integrations; no direct embedded database access.
3. **Python SDK:** agent-framework and evaluation integrations; streaming with bounded buffers.
4. **Go SDK:** sandbox, infrastructure, and server integrations.

Generated transport types are wrapped in hand-written domain types. SDKs validate obvious input
but never duplicate authorization or commit logic. All SDKs run the same black-box contract suite
against a daemon fixture.

## Compatibility and deprecation

- APIs are experimental until the first complete vertical slice passes conformance tests.
- Experimental fields are visibly namespaced and may change only in documented minor releases.
- Stable behavior is deprecated for at least two minor releases before removal.
- The server returns machine-readable deprecation notices.
- Export format support outlives RPC support so users can always recover data.
- Physical table names and SurrealQL are never part of the stable SDK contract.

## Observability

Every request emits latency, outcome, retry class, bytes, selected read commit, and domain
sequence. Logs include request and correlation IDs but not secrets, full file content, model
prompts, or unrestricted paths by default. Traces link operational spans to SurrealFS run/span IDs
without treating vendor tracing IDs as durable identity.

## API acceptance criteria

- A killed client can safely learn whether a timed-out command committed.
- Four SDKs produce identical canonical outcomes for the conformance corpus.
- Pagination is stable while a branch head changes concurrently.
- No ordinary API can create a commit without an author and mutation set.
- No captured workspace can publish without a valid daemon-issued capability, bound author span,
  expected head, and satisfied process policy.
- `close`/`fsync` never publish; abort and pre-commit crash expose no staged logical state.
- No ordinary client can issue a SurrealQL write to owned tables.
- Large files and exports remain within configured memory bounds.
- Old clients either work within negotiated capabilities or fail before mutation.
- An alternative storage adapter can pass the protocol suite without changing SDKs.
