# Security and Tenancy

## Security objective

SurrealFS stores unusually sensitive material: source code, credentials accidentally written to
files, prompts, model outputs, tool inputs, policy decisions, execution history, and a graph that
can reveal how an organization works. The security model must protect both content and the causal
relationships between content.

The primary objective is:

> No principal may read, mutate, infer, export, or subscribe to data outside its authorized scope,
> and no successful mutation may bypass provenance and policy capture.

## Trust boundaries

```text
untrusted agent/tool process
        |
        | local RPC with peer identity
        v
surrealfsd: parser -> authn -> authz -> policy -> limits -> semantic kernel
        |
        | private in-process SDK calls only
        v
embedded SurrealDB
        |
        v
SurrealKV directory + encryption/OS volume boundary
```

Agents, tool subprocesses, mount clients, SDK callers, imported archives, raw queries, and remote
peers are untrusted. `surrealfsd` and its pinned database engine are in the trusted computing base.
Administrative operators are powerful but audited; their clients are not implicitly trusted.

## Threat model

The design explicitly addresses:

- a compromised agent attempting to escape its repository or branch scope;
- path traversal, symlink, hard-link, and mount confusion attacks;
- injection through paths, record IDs, SurrealQL variables, import data, or metadata;
- one tenant inferring another through IDs, errors, timing, counts, subscriptions, or caches;
- bypassing provenance through direct database writes;
- forging span/trace context to claim another tool's writes;
- losing context across subprocesses or attributing a detached descendant to a completed tool;
- nested or concurrent tools reusing the wrong writable workspace;
- bypassing the workspace through a host path, writable lower mount, or database socket/directory;
- request replay, duplicate side effects, and confused-deputy calls;
- secret leakage in prompts, tool arguments, logs, diffs, exports, and graph previews;
- graph traversal amplification and unbounded file/chunk allocation;
- database-directory theft and offline inspection;
- malicious or corrupted backups and logical imports;
- compromised administrators or unauthorized recovery mode;
- tampering that rewrites causal history;
- denial of service through tiny-file storms, huge relations, long transactions, or slow streams;
- dependency and migration supply-chain attacks.

The initial local-first edition does not claim to defend against a root user controlling the host,
a fully compromised daemon process, physical memory extraction, or rollback of the entire storage
volume without an external trusted checkpoint.

## Identity

Every request executes as a typed principal:

- `human:<provider>/<subject>`;
- `service:<tenant>/<service>`;
- `agent:<repository>/<agent-instance>`;
- `operator:<deployment>/<subject>`;
- `migration:<signed-tool-version>`.

Human and service identity comes from verified transport credentials. Agent identities are minted
by the daemon for a bounded run and cannot self-assert a broader repository. Imported author names
are historical attributes, not authenticated principals.

Credentials are short-lived where possible. Tokens include issuer, audience, expiry, nonce/key ID,
tenant, allowed repositories, and coarse capabilities. The daemon validates time with a documented
clock-skew budget and supports key rotation.

## Workspace capability and process attribution

Trace context is untrusted correlation input. A `traceparent`, span ID, environment variable, or
caller-provided process ID never grants write authority. `surrealfsd` mints a random opaque workspace
capability after authenticating the launcher and binds its hash to repository, branch/base commit,
principal, author span, process scope, permissions, expiry, and nonce.

For the Linux proof:

1. the daemon/launcher creates a private mount namespace and OverlayFS upper/work directory;
2. the writable tool starts in a dedicated cgroup subtree and inherits the workspace endpoint and
   capability through the controlled launch path;
3. committed lower state and the database directory/socket are not writable or directly reachable;
4. descendants inherit the namespace/cgroup; the daemon uses recursive cgroup state to determine
   whether the process tree is quiescent;
5. publish requires the same live capability and process scope and refuses while forbidden
   descendants remain;
6. on timeout, the daemon terminates the workspace process tree and aborts staged state;
7. Landlock or an equivalent sandbox layer restricts ambient host filesystem access where available,
   as defense in depth rather than the sole boundary.

Initial policy is intentionally strict: missing context rejects writes; nested writable tools are
rejected or serialized; concurrent tools receive distinct workspaces; detached background processes
are unsupported. Observational child spans can share the parent's workspace but cannot independently
publish it. Platform adapters that cannot prove equivalent scoping are read-only or labeled with
reduced attribution guarantees.

Capability material is redacted from logs, traces, exports, and persistent records. Rotation,
revocation, expiry, replay, confused-deputy, descendant, and cross-workspace negative tests are
mandatory. `CAPTURED` is a security claim and cannot be assigned after a missing-context fallback.

## Authorization model

Authorization combines capabilities with scoped resources. Representative capabilities:

```text
repo.read_metadata       repo.read_content
repo.write_workspace     repo.move_branch
run.capture              run.read_sensitive
graph.query              graph.query_sensitive
artifact.export          repository.export
policy.approve           repository.admin
raw_query.read           recovery.unsafe_write
```

Evaluation uses the authenticated principal, tenant, repository, branch, commit, operation,
requested fields, run/span context, and current policy bundle. A deny is captured as a
`policy_decision` when the request reached policy evaluation, without storing a prohibited secret
payload.

POSIX mode bits are filesystem data and are enforced for mounted filesystem behavior. They do not
replace service authorization. A caller needs both the service capability and applicable POSIX
permission. This prevents a world-readable file in repository A from becoming visible to a caller
with no access to A.

## Tenant and repository isolation

Every tenant-owned record includes an immutable `tenant_id`; every repository-owned record also
includes `repository_id`. Relation endpoints must share tenant and, unless explicitly defined for
cross-repository lineage, repository. The semantic kernel checks this before writing an edge.

Defense in depth:

1. RPC handlers derive scope from the authenticated session.
2. Domain commands carry strongly typed scoped IDs.
3. Storage queries include tenant and repository predicates.
4. Composite indexes begin with tenant/repository where access patterns require it.
5. Schema assertions reject mismatched relation endpoints where feasible.
6. Result serialization performs field-level authorization.
7. Black-box tests attempt cross-tenant access for every endpoint.

Opaque IDs reduce accidental disclosure but are not an authorization control. `NOT_FOUND` is used
instead of distinguishable forbidden/existence responses where revealing existence is sensitive.

The first production milestone should use a separate database directory per trust domain or tenant
when operationally feasible. Shared-database multi-tenancy is accepted only after adversarial
isolation tests and query-plan review. A per-tenant database narrows blast radius but increases
upgrade, cache, backup, and fleet complexity; the deployment manifest records which model applies.

## Filesystem attack resistance

- Paths are parsed as data, never concatenated into database queries or host paths.
- Repository paths cannot contain NUL; `.` and `..` are resolved according to the documented path
  walker without escaping the repository root.
- Symlink traversal has an explicit maximum and loop detection.
- Host filesystem adapters use safe descriptor-relative operations where host paths are involved.
- Workspace lower/committed state is read-only; only the private overlay is writable by the tool.
- The database directory and daemon administrative endpoint are outside the tool's mount namespace
  and sandbox allowlist.
- Case sensitivity and Unicode normalization are repository properties; no implicit normalization
  changes identity.
- Hard links cannot cross repository boundaries.
- Device nodes and privileged special files are unsupported by default.
- Setuid/setgid behavior is stripped or virtualized; mounting SurrealFS must not grant host
  privilege.
- Archive import rejects absolute paths, parent traversal, device entries, oversized expansion,
  and duplicate-path ambiguity.

## Database access

The SurrealKV directory is readable/writable only by the daemon operating-system identity. The
daemon owns an exclusive lock. Ordinary clients never receive database credentials, a filesystem
path to the store, or an embedded SDK handle.

All application queries are parameterized. SurrealQL identifiers come from compile-time reviewed
queries or strict allowlists, not request strings. Raw read-only queries pass through a parser and
policy layer, have mandatory scope, and execute with resource limits. Raw writes require offline
recovery mode as defined in the API document.

## Secrets and sensitive data

SurrealFS cannot promise that agent data contains no secrets. It implements layered handling:

### Classification

Records and artifacts carry a data class such as `PUBLIC`, `INTERNAL`, `CONFIDENTIAL`, `SECRET`, or
`CREDENTIAL`. Classification may be supplied, inherited, detected, or overridden by policy. The
origin of a classification is recorded.

### Capture minimization

- SDK integrations capture hashes and typed summaries when raw payload is unnecessary.
- Environment variables are deny-by-default; named safe variables may be captured.
- Tool arguments have per-tool redaction schemas.
- Model prompts and outputs can be stored as encrypted artifacts or omitted while retaining hashes.
- Error objects and logs truncate and redact before persistence.

### Redaction

Redaction happens before a value enters general-purpose logs, indexes, live notifications, or
preview fields. The original, if retention is authorized, lives in a separately classified
content object. Redaction rules are versioned and their decision is linked to the captured record.
Redaction is not reversible masking; replacement values cannot reveal length or recognizable
prefixes unless policy explicitly allows it.

### Encryption

- TLS protects remote transport; local IPC relies on protected OS mechanisms and may add message
  encryption for hostile local environments.
- Storage-at-rest encryption initially relies on an approved encrypted volume unless the exact
  embedded stack offers a reviewed encryption mode.
- Highly sensitive artifacts should support application-level envelope encryption: one data key
  per object or pack, wrapped by a tenant key in KMS/keychain/HSM.
- Key IDs and algorithms are stored; plaintext keys never enter the database.
- Rotation re-wraps data keys without rewriting all content where possible.
- Content hashes of low-entropy secrets can enable guessing, so hashes are not treated as
  confidentiality. Sensitive deduplication may be tenant-scoped, keyed, or disabled.

## Content addressing and cross-tenant leakage

Global content-addressed deduplication creates an equality oracle: a tenant might infer that
another tenant has the same content. Initial multi-tenant deployments use tenant-scoped chunk IDs
derived from a keyed digest or maintain physically separate stores. Server responses never reveal
whether an upload deduplicated against another tenant.

For local single-tenant repositories, ordinary cryptographic hashes remain appropriate for
integrity. The hash algorithm is versioned so migration is possible if cryptographic guidance
changes.

## Policy enforcement

Policies evaluate before the commit transaction and, for state-dependent rules, again inside or
immediately adjacent to the authoritative head check. Inputs include canonical mutation summaries,
not caller descriptions.

Policy examples:

- deny modification under protected paths;
- require human approval before publishing an artifact;
- prevent secrets from entering a public branch;
- limit tool calls that report external side effects;
- require an evaluation threshold before advancing a release branch;
- restrict raw query fields and graph traversal depth;
- cap run spend, bytes, mutations, or wall time.

Every consequential decision stores policy bundle hash, rule IDs, input digest, outcome, reason,
approver if any, and resulting commit/request ID. Policy evaluation failure defaults to deny for
protected operations.

## External tool side effects

See [External effects and recovery](16-external-effects-and-recovery.md) for the canonical ledger,
dispatch, reconciliation, compensation, and recovery design. This section defines its security
boundary.

SurrealFS can atomically record a tool call and local state, but it cannot atomically transact with
email, cloud APIs, payment systems, or arbitrary shell effects. Tools must report an external
effect descriptor with provider, operation, target digest, provider idempotency key, and resulting
remote identifier where safe.

The platform uses an outbox/intention protocol for supported integrations and labels the outcome
`UNKNOWN` after ambiguous failures. Replay never blindly repeats an external effect. Policy can
require confirmation before retry.

## Audit integrity

Immutable application commits, mutation hashes, and state roots make accidental or unauthorized
rewrites detectable inside a repository, but a local attacker who controls the entire store could
replace all roots. Higher assurance deployments periodically anchor signed repository checkpoints
to an external append-only system or administrator-controlled transparency log.

Audit events include authentication, authorization, policy, administrative reads, exports,
migrations, recovery-mode entry, verification, and key operations. They have monotonic domain
sequences and chained hashes. Sensitive payloads are excluded; the audit proves an operation
occurred without becoming a second secret store.

## Resource isolation and denial of service

Quotas apply per tenant, repository, run, principal, and request as appropriate:

- maximum repositories, branches, open workspaces, and concurrent streams;
- maximum file, artifact, chunk, mutation, commit, and transaction sizes;
- maximum directory entries and graph fan-out returned per page;
- maximum query execution time, rows, depth, memory, and response bytes;
- maximum event rate and retained history;
- CPU, memory, disk, and bandwidth budgets;
- deadline and idle-timeout enforcement.

Chunk staging reserves quota before upload. Abandoned staging expires. Garbage collection is
incremental and bounded. Expensive verification and export jobs run in a controlled worker pool
and cannot starve commit traffic.

## Imports, exports, and backups

Imports are hostile input. They are streamed into a staging namespace/directory, checked for
format version, decompression limits, hashes, record schema, ID scope, graph endpoints, duplicate
identity, and state-root consistency. Nothing becomes visible until verification succeeds.

Exports require explicit capability, are audit logged, apply field/content policy, and are
encrypted for the recipient where needed. A logical export containing secrets retains data
classifications and cannot silently downgrade them.

Backup media is encrypted, access-controlled, inventoried, retention-limited, and tested for
restoration. Deleting live data is not complete until the applicable backup-retention contract is
satisfied; user-facing deletion semantics state this honestly.

## Supply chain and release security

- Pin Rust toolchain and every dependency.
- Generate an SBOM and provenance attestation for daemon releases.
- Verify migration and schema files by release signature/hash.
- Run vulnerability and license scanning, but do not treat scanners as proof of safety.
- Keep SurrealDB internal APIs out of application crates.
- Reproduce builds where practical.
- Separate development signing, release signing, and tenant data keys.
- Require review for changes to auth, path walking, raw queries, import, migrations, policy, and
  cryptographic code.

## Incident response

On suspected integrity or isolation failure:

1. stop new writes without deleting evidence;
2. preserve process, audit, database, configuration, and release metadata under access control;
3. identify affected tenants/repositories and last trusted checkpoint;
4. rotate credentials/keys where exposure is plausible;
5. verify state roots, audit chains, backups, and external checkpoint anchors;
6. restore into a separate target rather than mutating evidence;
7. communicate scope and uncertainty;
8. ship a tested fix and migration;
9. document root cause and add a regression/fault test.

The daemon offers a read-only quarantine mode for investigation. `INTEGRITY_FAILURE` is fail-closed
for writes.

## Security release gates

- Threat model reviewed for the actual deployment shape.
- Cross-tenant negative tests cover every API and subscription.
- Fuzzers cover path parsing, protocol decoding, imports, query parameters, and mutation decoding.
- No client-accessible route can open raw write mode.
- Forged trace/span IDs, missing/expired capabilities, cross-workspace handles, direct lower/database
  access, and detached descendants all fail closed in black-box tests.
- Process-tree propagation and quiescence tests cover exec, fork, crash, timeout, and cancellation.
- Secret scanning verifies logs, traces, error messages, previews, and test fixtures.
- Restore, key rotation, credential revocation, and quarantine drills succeed.
- Graph queries are bounded under adversarial high fan-out.
- At-rest controls and key ownership match customer promises.
- External penetration review is complete before shared multi-tenancy.
- License and dependency review is recorded for the pinned engine release.
