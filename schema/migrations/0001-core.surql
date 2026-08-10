-- SurrealFS migration 0001: core tables for the Phase 0–2 vertical slice.
-- Derived from schema/001-core.surql (design draft). Deliberate slice reductions:
--   * inode/dentry/file_extent/xattr_version/kv_version tables are deferred: the namespace
--     is a content-addressed tree whose nodes live in state_node (kind DIR), the KV map is a
--     single state_node (kind KV), and mutation evidence lives in commit_mutation. Row-level
--     projections are a later, measured migration.
--   * artifact/evaluation/policy/span_event/snapshot tables and their relations are
--     deferred to their phases (6/11/12).
--   * KV keys are UTF-8 strings in this version; KV values are content chunks.
-- The embedded store owner (surrealfs-core) applies migrations while holding the exclusive
-- process lock; ordinary callers never execute DDL. (The draft's surrealfsd-owned wording
-- predates the embedded-first decision.)

-- Note: schema_manifest and migration_receipt are defined by the migration runner itself
-- (idempotent IF NOT EXISTS bootstrap) so the checksum ledger can be read before any
-- migration has ever run.

DEFINE TABLE tenant TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD slug ON TABLE tenant TYPE string;
DEFINE FIELD created_at ON TABLE tenant TYPE datetime;

DEFINE TABLE repository TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE repository TYPE record<tenant>;
DEFINE FIELD slug ON TABLE repository TYPE string;
DEFINE FIELD path_mode ON TABLE repository TYPE string
    ASSERT $value IN ["BYTE_CASE_SENSITIVE"];
DEFINE FIELD hash_version ON TABLE repository TYPE int ASSERT $value > 0;
DEFINE FIELD root_format_version ON TABLE repository TYPE int ASSERT $value > 0;
DEFINE FIELD created_at ON TABLE repository TYPE datetime;
DEFINE INDEX repository_tenant_slug_unique ON TABLE repository FIELDS tenant, slug UNIQUE;

DEFINE TABLE principal TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE principal TYPE record<tenant>;
DEFINE FIELD kind ON TABLE principal TYPE string
    ASSERT $value IN ["HUMAN", "SERVICE", "AGENT", "OPERATOR", "MIGRATION"];
DEFINE FIELD external_subject ON TABLE principal TYPE string;
DEFINE FIELD created_at ON TABLE principal TYPE datetime;

DEFINE TABLE agent TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE agent TYPE record<repository>;
DEFINE FIELD stable_name ON TABLE agent TYPE string;
DEFINE FIELD created_at ON TABLE agent TYPE datetime;

DEFINE TABLE run TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE run TYPE record<repository>;
DEFINE FIELD agent ON TABLE run TYPE record<agent>;
DEFINE FIELD actor ON TABLE run TYPE record<principal>;
DEFINE FIELD status ON TABLE run TYPE string
    ASSERT $value IN ["RUNNING", "SUCCEEDED", "FAILED", "CANCELLED", "UNKNOWN"];
DEFINE FIELD started_at ON TABLE run TYPE datetime;
DEFINE FIELD finished_at ON TABLE run TYPE option<datetime>;
DEFINE INDEX run_repo_started ON TABLE run FIELDS repository, started_at;

DEFINE TABLE span TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE span TYPE record<repository>;
DEFINE FIELD run ON TABLE span TYPE record<run>;
DEFINE FIELD parent_span ON TABLE span TYPE option<record<span>>;
DEFINE FIELD kind ON TABLE span TYPE string
    ASSERT $value IN ["AGENT", "TOOL_CALL", "MODEL_CALL", "WORKSPACE", "EVALUATION", "CUSTOM"];
DEFINE FIELD name ON TABLE span TYPE string;
DEFINE FIELD status ON TABLE span TYPE string
    ASSERT $value IN ["RUNNING", "SUCCEEDED", "FAILED", "CANCELLED", "UNKNOWN"];
DEFINE FIELD capture_quality ON TABLE span TYPE string
    ASSERT $value IN ["CAPTURED", "IMPORTED", "INFERRED"];
DEFINE FIELD started_at ON TABLE span TYPE datetime;
DEFINE FIELD finished_at ON TABLE span TYPE option<datetime>;
DEFINE INDEX span_run_started ON TABLE span FIELDS run, started_at;

DEFINE TABLE tool_call TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE tool_call TYPE record<repository>;
DEFINE FIELD run ON TABLE tool_call TYPE record<run>;
DEFINE FIELD span ON TABLE tool_call TYPE record<span>;
DEFINE FIELD tool_name ON TABLE tool_call TYPE string;
DEFINE FIELD input_preview ON TABLE tool_call TYPE option<string>;
DEFINE FIELD output_preview ON TABLE tool_call TYPE option<string>;
DEFINE FIELD error_message ON TABLE tool_call TYPE option<string>;
DEFINE FIELD started_at ON TABLE tool_call TYPE datetime;
DEFINE FIELD finished_at ON TABLE tool_call TYPE option<datetime>;
DEFINE INDEX tool_call_repo_name ON TABLE tool_call FIELDS repository, tool_name;
DEFINE INDEX tool_call_repo_started ON TABLE tool_call FIELDS repository, started_at;

DEFINE TABLE state_node TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE state_node TYPE record<repository>;
DEFINE FIELD root_format_version ON TABLE state_node TYPE int ASSERT $value > 0;
DEFINE FIELD kind ON TABLE state_node TYPE string
    ASSERT $value IN ["DIR", "KV"];
DEFINE FIELD digest ON TABLE state_node TYPE string;
DEFINE FIELD body ON TABLE state_node TYPE object FLEXIBLE;
DEFINE FIELD created_at ON TABLE state_node TYPE datetime;

DEFINE TABLE state_root TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE state_root TYPE record<repository>;
DEFINE FIELD root_format_version ON TABLE state_root TYPE int ASSERT $value > 0;
DEFINE FIELD namespace_node ON TABLE state_root TYPE record<state_node>;
DEFINE FIELD kv_node ON TABLE state_root TYPE record<state_node>;
DEFINE FIELD digest ON TABLE state_root TYPE string;
DEFINE FIELD created_at ON TABLE state_root TYPE datetime;

DEFINE TABLE commit TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE commit TYPE record<repository>;
DEFINE FIELD kind ON TABLE commit TYPE string
    ASSERT $value IN ["ROOT", "NORMAL"];
DEFINE FIELD first_parent ON TABLE commit TYPE option<record<commit>>;
DEFINE FIELD author_principal ON TABLE commit TYPE record<principal>;
DEFINE FIELD author_span ON TABLE commit TYPE option<record<span>>;
DEFINE FIELD request_id ON TABLE commit TYPE string;
DEFINE FIELD message ON TABLE commit TYPE option<string>;
DEFINE FIELD mutation_count ON TABLE commit TYPE int ASSERT $value >= 0;
DEFINE FIELD state_root ON TABLE commit TYPE record<state_root>;
DEFINE FIELD state_root_digest ON TABLE commit TYPE string;
DEFINE FIELD hash_version ON TABLE commit TYPE int ASSERT $value > 0;
DEFINE FIELD domain_sequence ON TABLE commit TYPE int ASSERT $value >= 0;
DEFINE FIELD committed_at ON TABLE commit TYPE datetime;
DEFINE INDEX commit_repo_sequence_unique ON TABLE commit FIELDS repository, domain_sequence UNIQUE;
DEFINE INDEX commit_repo_time ON TABLE commit FIELDS repository, committed_at;

DEFINE TABLE branch TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE branch TYPE record<repository>;
DEFINE FIELD name ON TABLE branch TYPE string;
DEFINE FIELD head ON TABLE branch TYPE record<commit>;
DEFINE FIELD created_at ON TABLE branch TYPE datetime;
DEFINE FIELD updated_at ON TABLE branch TYPE datetime;

DEFINE TABLE workspace TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE workspace TYPE record<repository>;
DEFINE FIELD branch ON TABLE workspace TYPE record<branch>;
DEFINE FIELD base_commit ON TABLE workspace TYPE record<commit>;
DEFINE FIELD author_principal ON TABLE workspace TYPE record<principal>;
DEFINE FIELD status ON TABLE workspace TYPE string
    ASSERT $value IN ["OPEN", "COMMITTED", "ABORTED"];
DEFINE FIELD final_commit ON TABLE workspace TYPE option<record<commit>>;
DEFINE FIELD abort_reason ON TABLE workspace TYPE option<string>;
DEFINE FIELD created_at ON TABLE workspace TYPE datetime;
DEFINE FIELD closed_at ON TABLE workspace TYPE option<datetime>;
DEFINE INDEX workspace_repo_status ON TABLE workspace FIELDS repository, status;

DEFINE TABLE commit_mutation TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE commit_mutation TYPE record<repository>;
DEFINE FIELD commit ON TABLE commit_mutation TYPE record<commit>;
DEFINE FIELD ordinal ON TABLE commit_mutation TYPE int ASSERT $value >= 0;
DEFINE FIELD kind ON TABLE commit_mutation TYPE string;
-- The path this mutation touched, as a queryable column rather than a value buried in the
-- JSON body. This is what turns "which tool call last wrote this file" into a graph
-- traversal instead of a scan-and-parse over every mutation ever recorded.
DEFINE FIELD path ON TABLE commit_mutation TYPE string;
DEFINE FIELD body ON TABLE commit_mutation TYPE object FLEXIBLE;
DEFINE INDEX mutation_repo_commit ON TABLE commit_mutation FIELDS repository, commit;
DEFINE INDEX mutation_repo_path ON TABLE commit_mutation FIELDS repository, path;

DEFINE TABLE request_receipt TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE request_receipt TYPE record<repository>;
DEFINE FIELD request_id ON TABLE request_receipt TYPE string;
DEFINE FIELD command_hash ON TABLE request_receipt TYPE string;
DEFINE FIELD outcome ON TABLE request_receipt TYPE string
    ASSERT $value IN ["APPLIED"];
DEFINE FIELD commit ON TABLE request_receipt TYPE record<commit>;
DEFINE FIELD state_root_digest ON TABLE request_receipt TYPE string;
DEFINE FIELD previous_head ON TABLE request_receipt TYPE record<commit>;
DEFINE FIELD domain_sequence ON TABLE request_receipt TYPE int ASSERT $value >= 0;
DEFINE FIELD created_at ON TABLE request_receipt TYPE datetime;

DEFINE TABLE chunk TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE chunk TYPE record<repository>;
DEFINE FIELD hash_version ON TABLE chunk TYPE int ASSERT $value > 0;
DEFINE FIELD digest ON TABLE chunk TYPE string;
DEFINE FIELD length ON TABLE chunk TYPE int ASSERT $value >= 0;
DEFINE FIELD storage_kind ON TABLE chunk TYPE string ASSERT $value IN ["INLINE"];
DEFINE FIELD inline_bytes ON TABLE chunk TYPE bytes;
DEFINE FIELD created_at ON TABLE chunk TYPE datetime;

DEFINE TABLE parent_of TYPE RELATION FROM commit TO commit SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE parent_of TYPE record<repository>;

DEFINE TABLE caused TYPE RELATION FROM span | tool_call TO commit SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE caused TYPE record<repository>;

DEFINE TABLE owns_workspace TYPE RELATION FROM span | tool_call TO workspace SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE owns_workspace TYPE record<repository>;

DEFINE TABLE based_on TYPE RELATION FROM workspace TO commit SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE based_on TYPE record<repository>;

DEFINE TABLE published_as TYPE RELATION FROM workspace TO commit SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE published_as TYPE record<repository>;
