# Glossary

**Agent** — A configured autonomous or semi-autonomous actor that initiates runs.

**Artifact** — A semantically meaningful output or input identified independently of its path, such
as a report, patch, binary, dataset, or generated image.

**Branch** — A named mutable reference to a commit, with ancestry and optional base branch metadata.

**Cause** — The run span, process, import, merge, administrative operation, or system action assigned
responsibility for a commit.

**Chunk** — An immutable content-addressed byte sequence used to store file or artifact content.

**Commit** — An immutable atomic logical state transition with parent references, mutations, hashes,
and causal metadata.

**Current state** — Materialized records used for fast reads at a branch head.

**Dentry** — A directory entry mapping `(branch, parent inode, name)` to an inode.

**Evaluation** — A recorded judgment, score, assertion, or comparison applied to a run, artifact,
commit, or branch.

**Execution graph** — Records and relations connecting agents, runs, spans, tools, commits, state,
artifacts, policies, evaluations, and forks.

**Extent** — A mapping from a file byte range to a chunk byte range or a hole.

**Fork** — Creation of a branch whose initial state is an existing commit without copying all state.

**Historical state** — Immutable state versions retained for commit-based reads and reconstruction.

**Inode** — A stable filesystem object identity carrying type and metadata independently of names.

**Logical export** — A backend-independent representation of SurrealFS records, relations, and
content, suitable for verification and import.

**Materialized head** — Current-state records maintained transactionally for efficient branch reads.

**Mutation** — One ordered logical change within a commit, such as create dentry, replace extents, or
set a KV value.

**Policy decision** — A recorded allow, deny, require-approval, redact, or constrain result tied to
the action and evidence evaluated.

**Repository** — The isolation, identity, retention, and branching boundary for one SurrealFS state
universe.

**Run** — A top-level attempt by an agent to accomplish a goal from a defined starting state.

**Snapshot** — A named immutable reference to a commit.

**Span** — A nested interval of causal work within a run. Tool calls and process executions are span
types.

**State root** — A deterministic digest representing the logical filesystem and KV state at a
commit. Its implementation may evolve by version, but verification rules are fixed per version.

**SurrealFS-owned record** — A table or relation whose invariants may be changed only through the
SurrealFS domain API and schema migrations.

**Tool call** — A span representing an invocation with structured input, result/error, and timing.

**Versioned storage** — SurrealDB/SurrealKV engine-level historical read support. It is useful but is
not identical to SurrealFS commits, branches, or retention.

