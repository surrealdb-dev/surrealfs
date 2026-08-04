-- SurrealFS core schema design draft
-- Target: the pinned SurrealDB 3.x development baseline described in docs/08.
-- Status: NOT an applied production migration. Validate this file against the exact pinned
-- engine, split it into numbered forward migrations, and add schema tests before implementation.
--
-- Ordinary clients must never execute this file. surrealfsd owns the namespace/database and
-- applies signed, checksum-tracked migrations while in maintenance mode.

-- -----------------------------------------------------------------------------
-- Service and repository identity
-- -----------------------------------------------------------------------------

DEFINE TABLE schema_manifest TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD schema_version ON TABLE schema_manifest TYPE int ASSERT $value >= 0;
DEFINE FIELD export_version ON TABLE schema_manifest TYPE int ASSERT $value >= 0;
DEFINE FIELD engine_revision ON TABLE schema_manifest TYPE string;
DEFINE FIELD migration_hash ON TABLE schema_manifest TYPE string;
DEFINE FIELD applied_at ON TABLE schema_manifest TYPE datetime;

DEFINE TABLE tenant TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD slug ON TABLE tenant TYPE string;
DEFINE FIELD display_name ON TABLE tenant TYPE string;
DEFINE FIELD created_at ON TABLE tenant TYPE datetime;
DEFINE FIELD data_class_default ON TABLE tenant TYPE string
    ASSERT $value IN ["PUBLIC", "INTERNAL", "CONFIDENTIAL", "SECRET", "CREDENTIAL"];
DEFINE INDEX tenant_slug_unique ON TABLE tenant FIELDS slug UNIQUE;

DEFINE TABLE repository TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE repository TYPE record<tenant>;
DEFINE FIELD slug ON TABLE repository TYPE string;
DEFINE FIELD display_name ON TABLE repository TYPE string;
DEFINE FIELD path_mode ON TABLE repository TYPE string
    ASSERT $value IN ["BYTE_CASE_SENSITIVE", "UTF8_CASE_SENSITIVE", "UTF8_CASE_INSENSITIVE"];
DEFINE FIELD hash_version ON TABLE repository TYPE int ASSERT $value > 0;
DEFINE FIELD created_at ON TABLE repository TYPE datetime;
DEFINE FIELD archived_at ON TABLE repository TYPE option<datetime>;
DEFINE INDEX repository_tenant_slug_unique ON TABLE repository FIELDS tenant, slug UNIQUE;

DEFINE TABLE principal TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE principal TYPE record<tenant>;
DEFINE FIELD kind ON TABLE principal TYPE string
    ASSERT $value IN ["HUMAN", "SERVICE", "AGENT", "OPERATOR", "MIGRATION"];
DEFINE FIELD external_subject ON TABLE principal TYPE string;
DEFINE FIELD display_name ON TABLE principal TYPE option<string>;
DEFINE FIELD created_at ON TABLE principal TYPE datetime;
DEFINE FIELD disabled_at ON TABLE principal TYPE option<datetime>;
DEFINE INDEX principal_external_unique ON TABLE principal FIELDS tenant, kind, external_subject UNIQUE;

-- -----------------------------------------------------------------------------
-- Execution records
-- -----------------------------------------------------------------------------

DEFINE TABLE agent TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE agent TYPE record<tenant>;
DEFINE FIELD repository ON TABLE agent TYPE record<repository>;
DEFINE FIELD stable_name ON TABLE agent TYPE string;
DEFINE FIELD framework ON TABLE agent TYPE option<string>;
DEFINE FIELD definition_hash ON TABLE agent TYPE option<string>;
DEFINE FIELD created_at ON TABLE agent TYPE datetime;
DEFINE INDEX agent_repo_name_unique ON TABLE agent FIELDS repository, stable_name UNIQUE;

DEFINE TABLE run TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE run TYPE record<tenant>;
DEFINE FIELD repository ON TABLE run TYPE record<repository>;
DEFINE FIELD agent ON TABLE run TYPE record<agent>;
DEFINE FIELD actor ON TABLE run TYPE record<principal>;
DEFINE FIELD objective ON TABLE run TYPE option<string>;
DEFINE FIELD objective_hash ON TABLE run TYPE option<string>;
DEFINE FIELD status ON TABLE run TYPE string
    ASSERT $value IN ["RUNNING", "SUCCEEDED", "FAILED", "CANCELLED", "UNKNOWN"];
DEFINE FIELD capture_quality ON TABLE run TYPE string
    ASSERT $value IN ["CAPTURED", "IMPORTED", "INFERRED"];
DEFINE FIELD environment_manifest ON TABLE run TYPE option<object> FLEXIBLE;
DEFINE FIELD started_at ON TABLE run TYPE datetime;
DEFINE FIELD finished_at ON TABLE run TYPE option<datetime>;
DEFINE FIELD domain_sequence ON TABLE run TYPE int ASSERT $value >= 0;
DEFINE INDEX run_repo_started ON TABLE run FIELDS repository, started_at;
DEFINE INDEX run_repo_status ON TABLE run FIELDS repository, status, started_at;

DEFINE TABLE span TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE span TYPE record<tenant>;
DEFINE FIELD repository ON TABLE span TYPE record<repository>;
DEFINE FIELD run ON TABLE span TYPE record<run>;
DEFINE FIELD parent_span ON TABLE span TYPE option<record<span>>;
DEFINE FIELD kind ON TABLE span TYPE string
    ASSERT $value IN ["AGENT", "TOOL_CALL", "MODEL_CALL", "WORKSPACE", "EVALUATION", "CUSTOM"];
DEFINE FIELD name ON TABLE span TYPE string;
DEFINE FIELD status ON TABLE span TYPE string
    ASSERT $value IN ["RUNNING", "SUCCEEDED", "FAILED", "CANCELLED", "UNKNOWN"];
DEFINE FIELD input_manifest ON TABLE span TYPE option<object> FLEXIBLE;
DEFINE FIELD output_manifest ON TABLE span TYPE option<object> FLEXIBLE;
DEFINE FIELD capture_quality ON TABLE span TYPE string
    ASSERT $value IN ["CAPTURED", "IMPORTED", "INFERRED"];
DEFINE FIELD started_at ON TABLE span TYPE datetime;
DEFINE FIELD finished_at ON TABLE span TYPE option<datetime>;
DEFINE INDEX span_run_started ON TABLE span FIELDS run, started_at;
DEFINE INDEX span_parent_started ON TABLE span FIELDS parent_span, started_at;

DEFINE TABLE tool_call TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE tool_call TYPE record<tenant>;
DEFINE FIELD repository ON TABLE tool_call TYPE record<repository>;
DEFINE FIELD run ON TABLE tool_call TYPE record<run>;
DEFINE FIELD span ON TABLE tool_call TYPE record<span>;
DEFINE FIELD tool_name ON TABLE tool_call TYPE string;
DEFINE FIELD tool_version ON TABLE tool_call TYPE option<string>;
DEFINE FIELD input_hash ON TABLE tool_call TYPE option<string>;
DEFINE FIELD output_hash ON TABLE tool_call TYPE option<string>;
DEFINE FIELD external_effect_status ON TABLE tool_call TYPE string
    ASSERT $value IN ["NONE", "INTENDED", "SUCCEEDED", "FAILED", "UNKNOWN"];
DEFINE FIELD provider_idempotency_key_hash ON TABLE tool_call TYPE option<string>;
DEFINE INDEX tool_call_span_unique ON TABLE tool_call FIELDS span UNIQUE;
DEFINE INDEX tool_call_repo_name ON TABLE tool_call FIELDS repository, tool_name;

DEFINE TABLE span_event TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE span_event TYPE record<tenant>;
DEFINE FIELD repository ON TABLE span_event TYPE record<repository>;
DEFINE FIELD run ON TABLE span_event TYPE record<run>;
DEFINE FIELD span ON TABLE span_event TYPE record<span>;
DEFINE FIELD caller_sequence ON TABLE span_event TYPE int ASSERT $value >= 0;
DEFINE FIELD domain_sequence ON TABLE span_event TYPE int ASSERT $value >= 0;
DEFINE FIELD kind ON TABLE span_event TYPE string;
DEFINE FIELD payload_hash ON TABLE span_event TYPE option<string>;
DEFINE FIELD preview ON TABLE span_event TYPE option<string>;
DEFINE FIELD occurred_at ON TABLE span_event TYPE datetime;
DEFINE INDEX span_event_span_sequence_unique ON TABLE span_event FIELDS span, caller_sequence UNIQUE;
DEFINE INDEX span_event_repo_domain_sequence_unique ON TABLE span_event FIELDS repository, domain_sequence UNIQUE;

-- -----------------------------------------------------------------------------
-- Commit graph and branch state
-- -----------------------------------------------------------------------------

-- Canonical persistent state. IDs are derived from repository + format/kind + digest. The body is
-- versioned canonical node payload; application validation ensures children belong to the same
-- repository and their digests match. Optional branch-head projections are not defined here.
DEFINE TABLE state_node TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE state_node TYPE record<tenant>;
DEFINE FIELD repository ON TABLE state_node TYPE record<repository>;
DEFINE FIELD root_format_version ON TABLE state_node TYPE int ASSERT $value > 0;
DEFINE FIELD kind ON TABLE state_node TYPE string
    ASSERT $value IN ["NAMESPACE", "INODE", "EXTENT", "KV"];
DEFINE FIELD digest ON TABLE state_node TYPE string;
DEFINE FIELD children ON TABLE state_node TYPE array<record<state_node>>;
DEFINE FIELD body ON TABLE state_node TYPE object FLEXIBLE;
DEFINE FIELD created_at ON TABLE state_node TYPE datetime;
DEFINE INDEX state_node_digest_unique ON TABLE state_node FIELDS repository, root_format_version, kind, digest UNIQUE;

DEFINE TABLE state_root TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE state_root TYPE record<tenant>;
DEFINE FIELD repository ON TABLE state_root TYPE record<repository>;
DEFINE FIELD root_format_version ON TABLE state_root TYPE int ASSERT $value > 0;
DEFINE FIELD namespace_root ON TABLE state_root TYPE record<state_node>;
DEFINE FIELD inode_root ON TABLE state_root TYPE record<state_node>;
DEFINE FIELD extent_root ON TABLE state_root TYPE record<state_node>;
DEFINE FIELD kv_root ON TABLE state_root TYPE record<state_node>;
DEFINE FIELD digest ON TABLE state_root TYPE string;
DEFINE FIELD created_at ON TABLE state_root TYPE datetime;
DEFINE INDEX state_root_digest_unique ON TABLE state_root FIELDS repository, root_format_version, digest UNIQUE;

DEFINE TABLE commit TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE commit TYPE record<tenant>;
DEFINE FIELD repository ON TABLE commit TYPE record<repository>;
DEFINE FIELD kind ON TABLE commit TYPE string
    ASSERT $value IN ["ROOT", "NORMAL", "MERGE", "IMPORT", "POLICY", "SYSTEM"];
DEFINE FIELD first_parent ON TABLE commit TYPE option<record<commit>>;
DEFINE FIELD generation ON TABLE commit TYPE int ASSERT $value >= 0;
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
DEFINE INDEX commit_repo_request_unique ON TABLE commit FIELDS repository, request_id UNIQUE;
DEFINE INDEX commit_repo_domain_sequence_unique ON TABLE commit FIELDS repository, domain_sequence UNIQUE;
DEFINE INDEX commit_repo_time ON TABLE commit FIELDS repository, committed_at;
DEFINE INDEX commit_first_parent ON TABLE commit FIELDS repository, first_parent;

DEFINE TABLE branch TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE branch TYPE record<tenant>;
DEFINE FIELD repository ON TABLE branch TYPE record<repository>;
DEFINE FIELD name ON TABLE branch TYPE string;
DEFINE FIELD head ON TABLE branch TYPE record<commit>;
DEFINE FIELD head_generation ON TABLE branch TYPE int ASSERT $value >= 0;
DEFINE FIELD protected ON TABLE branch TYPE bool;
DEFINE FIELD created_from ON TABLE branch TYPE record<commit>;
DEFINE FIELD created_at ON TABLE branch TYPE datetime;
DEFINE FIELD updated_at ON TABLE branch TYPE datetime;
DEFINE INDEX branch_repo_name_unique ON TABLE branch FIELDS repository, name UNIQUE;
DEFINE INDEX branch_repo_head ON TABLE branch FIELDS repository, head;

-- Workspace metadata stores only a capability hash/identifier, never the bearer capability. The
-- private overlay itself is transient staging; only a published state root becomes canonical.
DEFINE TABLE workspace TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE workspace TYPE record<tenant>;
DEFINE FIELD repository ON TABLE workspace TYPE record<repository>;
DEFINE FIELD branch ON TABLE workspace TYPE record<branch>;
DEFINE FIELD base_commit ON TABLE workspace TYPE record<commit>;
DEFINE FIELD base_root ON TABLE workspace TYPE record<state_root>;
DEFINE FIELD author_principal ON TABLE workspace TYPE record<principal>;
DEFINE FIELD author_span ON TABLE workspace TYPE record<span>;
DEFINE FIELD capability_hash ON TABLE workspace TYPE string;
DEFINE FIELD process_scope_hash ON TABLE workspace TYPE string;
DEFINE FIELD isolation_backend ON TABLE workspace TYPE string;
DEFINE FIELD status ON TABLE workspace TYPE string
    ASSERT $value IN ["OPEN", "PUBLISHING", "COMMITTED", "ABORTED", "EXPIRED"];
DEFINE FIELD final_commit ON TABLE workspace TYPE option<record<commit>>;
DEFINE FIELD abort_reason ON TABLE workspace TYPE option<string>;
DEFINE FIELD created_at ON TABLE workspace TYPE datetime;
DEFINE FIELD expires_at ON TABLE workspace TYPE datetime;
DEFINE FIELD closed_at ON TABLE workspace TYPE option<datetime>;
DEFINE INDEX workspace_capability_unique ON TABLE workspace FIELDS repository, capability_hash UNIQUE;
DEFINE INDEX workspace_repo_status ON TABLE workspace FIELDS repository, status, expires_at;

DEFINE TABLE snapshot TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE snapshot TYPE record<tenant>;
DEFINE FIELD repository ON TABLE snapshot TYPE record<repository>;
DEFINE FIELD name ON TABLE snapshot TYPE string;
DEFINE FIELD commit ON TABLE snapshot TYPE record<commit>;
DEFINE FIELD created_by ON TABLE snapshot TYPE record<principal>;
DEFINE FIELD created_at ON TABLE snapshot TYPE datetime;
DEFINE FIELD expires_at ON TABLE snapshot TYPE option<datetime>;
DEFINE INDEX snapshot_repo_name_unique ON TABLE snapshot FIELDS repository, name UNIQUE;
DEFINE INDEX snapshot_repo_commit ON TABLE snapshot FIELDS repository, commit;

DEFINE TABLE commit_mutation TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE commit_mutation TYPE record<tenant>;
DEFINE FIELD repository ON TABLE commit_mutation TYPE record<repository>;
DEFINE FIELD commit ON TABLE commit_mutation TYPE record<commit>;
DEFINE FIELD ordinal ON TABLE commit_mutation TYPE int ASSERT $value >= 0;
DEFINE FIELD kind ON TABLE commit_mutation TYPE string;
DEFINE FIELD target_kind ON TABLE commit_mutation TYPE string;
DEFINE FIELD target_id ON TABLE commit_mutation TYPE string;
DEFINE FIELD path_before ON TABLE commit_mutation TYPE option<bytes>;
DEFINE FIELD path_after ON TABLE commit_mutation TYPE option<bytes>;
DEFINE FIELD before_hash ON TABLE commit_mutation TYPE option<string>;
DEFINE FIELD after_hash ON TABLE commit_mutation TYPE option<string>;
DEFINE FIELD body ON TABLE commit_mutation TYPE object FLEXIBLE;
DEFINE INDEX mutation_commit_ordinal_unique ON TABLE commit_mutation FIELDS commit, ordinal UNIQUE;
DEFINE INDEX mutation_repo_target ON TABLE commit_mutation FIELDS repository, target_kind, target_id;

DEFINE TABLE request_receipt TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE request_receipt TYPE record<tenant>;
DEFINE FIELD repository ON TABLE request_receipt TYPE record<repository>;
DEFINE FIELD request_id ON TABLE request_receipt TYPE string;
DEFINE FIELD command_hash ON TABLE request_receipt TYPE string;
DEFINE FIELD outcome ON TABLE request_receipt TYPE string
    ASSERT $value IN ["APPLIED", "REPLAYED", "CONFLICT", "REJECTED"];
DEFINE FIELD commit ON TABLE request_receipt TYPE option<record<commit>>;
DEFINE FIELD state_root ON TABLE request_receipt TYPE option<record<state_root>>;
DEFINE FIELD state_root_digest ON TABLE request_receipt TYPE option<string>;
DEFINE FIELD previous_head ON TABLE request_receipt TYPE option<record<commit>>;
DEFINE FIELD new_head ON TABLE request_receipt TYPE option<record<commit>>;
DEFINE FIELD domain_sequence ON TABLE request_receipt TYPE option<int>;
DEFINE FIELD durability_profile ON TABLE request_receipt TYPE string;
DEFINE FIELD created_at ON TABLE request_receipt TYPE datetime;
DEFINE INDEX receipt_repo_request_unique ON TABLE request_receipt FIELDS repository, request_id UNIQUE;

-- -----------------------------------------------------------------------------
-- Filesystem and KV state
--
-- These records are immutable values/evidence introduced by commits and referenced by persistent
-- state nodes. Hot reads may use additional root-keyed materialized-head tables added by a later
-- measured migration. Such projections are disposable and never define historical/current identity.
-- -----------------------------------------------------------------------------

DEFINE TABLE inode TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE inode TYPE record<tenant>;
DEFINE FIELD repository ON TABLE inode TYPE record<repository>;
DEFINE FIELD stable_inode_id ON TABLE inode TYPE string;
DEFINE FIELD introduced_by ON TABLE inode TYPE record<commit>;
DEFINE FIELD kind ON TABLE inode TYPE string
    ASSERT $value IN ["REGULAR", "DIRECTORY", "SYMLINK"];
DEFINE FIELD mode ON TABLE inode TYPE int ASSERT $value >= 0;
DEFINE FIELD uid ON TABLE inode TYPE int ASSERT $value >= 0;
DEFINE FIELD gid ON TABLE inode TYPE int ASSERT $value >= 0;
DEFINE FIELD size ON TABLE inode TYPE int ASSERT $value >= 0;
DEFINE FIELD link_count ON TABLE inode TYPE int ASSERT $value >= 0;
DEFINE FIELD content_root ON TABLE inode TYPE option<string>;
DEFINE FIELD metadata_hash ON TABLE inode TYPE string;
DEFINE FIELD modified_at ON TABLE inode TYPE datetime;
DEFINE INDEX inode_repo_stable_commit_unique ON TABLE inode FIELDS repository, stable_inode_id, introduced_by UNIQUE;
DEFINE INDEX inode_repo_kind ON TABLE inode FIELDS repository, kind;

DEFINE TABLE dentry TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE dentry TYPE record<tenant>;
DEFINE FIELD repository ON TABLE dentry TYPE record<repository>;
DEFINE FIELD parent_inode_id ON TABLE dentry TYPE string;
DEFINE FIELD name ON TABLE dentry TYPE bytes;
DEFINE FIELD child_inode_id ON TABLE dentry TYPE string;
DEFINE FIELD introduced_by ON TABLE dentry TYPE record<commit>;
DEFINE FIELD tombstone ON TABLE dentry TYPE bool;
DEFINE FIELD entry_hash ON TABLE dentry TYPE string;
DEFINE INDEX dentry_lookup ON TABLE dentry FIELDS repository, parent_inode_id, name, introduced_by;
DEFINE INDEX dentry_child ON TABLE dentry FIELDS repository, child_inode_id, introduced_by;

DEFINE TABLE file_extent TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE file_extent TYPE record<tenant>;
DEFINE FIELD repository ON TABLE file_extent TYPE record<repository>;
DEFINE FIELD stable_inode_id ON TABLE file_extent TYPE string;
DEFINE FIELD introduced_by ON TABLE file_extent TYPE record<commit>;
DEFINE FIELD ordinal ON TABLE file_extent TYPE int ASSERT $value >= 0;
DEFINE FIELD file_offset ON TABLE file_extent TYPE int ASSERT $value >= 0;
DEFINE FIELD length ON TABLE file_extent TYPE int ASSERT $value > 0;
DEFINE FIELD kind ON TABLE file_extent TYPE string ASSERT $value IN ["CHUNK", "HOLE"];
DEFINE FIELD chunk ON TABLE file_extent TYPE option<record<chunk>>;
DEFINE FIELD chunk_offset ON TABLE file_extent TYPE option<int>;
DEFINE INDEX extent_inode_ordinal_unique ON TABLE file_extent FIELDS repository, stable_inode_id, introduced_by, ordinal UNIQUE;

DEFINE TABLE chunk TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE chunk TYPE record<tenant>;
DEFINE FIELD repository ON TABLE chunk TYPE record<repository>;
DEFINE FIELD hash_version ON TABLE chunk TYPE int ASSERT $value > 0;
DEFINE FIELD digest ON TABLE chunk TYPE string;
DEFINE FIELD length ON TABLE chunk TYPE int ASSERT $value >= 0;
DEFINE FIELD storage_kind ON TABLE chunk TYPE string ASSERT $value IN ["INLINE", "PACK"];
DEFINE FIELD inline_bytes ON TABLE chunk TYPE option<bytes>;
DEFINE FIELD pack_id ON TABLE chunk TYPE option<string>;
DEFINE FIELD pack_offset ON TABLE chunk TYPE option<int>;
DEFINE FIELD staged_by ON TABLE chunk TYPE option<string>;
DEFINE FIELD state ON TABLE chunk TYPE string ASSERT $value IN ["STAGED", "VISIBLE", "TOMBSTONED"];
DEFINE FIELD created_at ON TABLE chunk TYPE datetime;
DEFINE INDEX chunk_repo_digest_unique ON TABLE chunk FIELDS repository, hash_version, digest UNIQUE;
DEFINE INDEX chunk_repo_state ON TABLE chunk FIELDS repository, state, created_at;

DEFINE TABLE xattr_version TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE xattr_version TYPE record<tenant>;
DEFINE FIELD repository ON TABLE xattr_version TYPE record<repository>;
DEFINE FIELD stable_inode_id ON TABLE xattr_version TYPE string;
DEFINE FIELD name ON TABLE xattr_version TYPE bytes;
DEFINE FIELD value ON TABLE xattr_version TYPE option<bytes>;
DEFINE FIELD introduced_by ON TABLE xattr_version TYPE record<commit>;
DEFINE FIELD tombstone ON TABLE xattr_version TYPE bool;
DEFINE INDEX xattr_lookup ON TABLE xattr_version FIELDS repository, stable_inode_id, name, introduced_by;

DEFINE TABLE kv_version TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE kv_version TYPE record<tenant>;
DEFINE FIELD repository ON TABLE kv_version TYPE record<repository>;
DEFINE FIELD namespace ON TABLE kv_version TYPE bytes;
DEFINE FIELD key ON TABLE kv_version TYPE bytes;
DEFINE FIELD value ON TABLE kv_version TYPE option<bytes>;
DEFINE FIELD value_hash ON TABLE kv_version TYPE option<string>;
DEFINE FIELD media_type ON TABLE kv_version TYPE option<string>;
DEFINE FIELD introduced_by ON TABLE kv_version TYPE record<commit>;
DEFINE FIELD author_span ON TABLE kv_version TYPE option<record<span>>;
DEFINE FIELD tombstone ON TABLE kv_version TYPE bool;
DEFINE INDEX kv_lookup ON TABLE kv_version FIELDS repository, namespace, key, introduced_by;

-- -----------------------------------------------------------------------------
-- Artifacts, evaluation, and policy
-- -----------------------------------------------------------------------------

DEFINE TABLE artifact TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE artifact TYPE record<tenant>;
DEFINE FIELD repository ON TABLE artifact TYPE record<repository>;
DEFINE FIELD kind ON TABLE artifact TYPE string;
DEFINE FIELD name ON TABLE artifact TYPE option<string>;
DEFINE FIELD media_type ON TABLE artifact TYPE option<string>;
DEFINE FIELD size ON TABLE artifact TYPE int ASSERT $value >= 0;
DEFINE FIELD content_hash ON TABLE artifact TYPE string;
DEFINE FIELD manifest ON TABLE artifact TYPE object FLEXIBLE;
DEFINE FIELD data_class ON TABLE artifact TYPE string
    ASSERT $value IN ["PUBLIC", "INTERNAL", "CONFIDENTIAL", "SECRET", "CREDENTIAL"];
DEFINE FIELD capture_quality ON TABLE artifact TYPE string
    ASSERT $value IN ["CAPTURED", "IMPORTED", "INFERRED"];
DEFINE FIELD created_at ON TABLE artifact TYPE datetime;
DEFINE INDEX artifact_repo_hash ON TABLE artifact FIELDS repository, content_hash;

DEFINE TABLE evaluation TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE evaluation TYPE record<tenant>;
DEFINE FIELD repository ON TABLE evaluation TYPE record<repository>;
DEFINE FIELD name ON TABLE evaluation TYPE string;
DEFINE FIELD evaluator ON TABLE evaluation TYPE string;
DEFINE FIELD evaluator_version ON TABLE evaluation TYPE string;
DEFINE FIELD subject_kind ON TABLE evaluation TYPE string;
DEFINE FIELD subject_id ON TABLE evaluation TYPE string;
DEFINE FIELD score ON TABLE evaluation TYPE option<number>;
DEFINE FIELD passed ON TABLE evaluation TYPE option<bool>;
DEFINE FIELD evidence ON TABLE evaluation TYPE option<object> FLEXIBLE;
DEFINE FIELD created_at ON TABLE evaluation TYPE datetime;
DEFINE INDEX evaluation_repo_subject ON TABLE evaluation FIELDS repository, subject_kind, subject_id;
DEFINE INDEX evaluation_repo_name_score ON TABLE evaluation FIELDS repository, name, score;

DEFINE TABLE policy_decision TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE policy_decision TYPE record<tenant>;
DEFINE FIELD repository ON TABLE policy_decision TYPE record<repository>;
DEFINE FIELD request_id ON TABLE policy_decision TYPE string;
DEFINE FIELD policy_bundle_hash ON TABLE policy_decision TYPE string;
DEFINE FIELD rule_ids ON TABLE policy_decision TYPE array<string>;
DEFINE FIELD input_hash ON TABLE policy_decision TYPE string;
DEFINE FIELD outcome ON TABLE policy_decision TYPE string
    ASSERT $value IN ["ALLOW", "DENY", "REQUIRE_APPROVAL", "ERROR"];
DEFINE FIELD reason ON TABLE policy_decision TYPE option<string>;
DEFINE FIELD actor ON TABLE policy_decision TYPE record<principal>;
DEFINE FIELD approver ON TABLE policy_decision TYPE option<record<principal>>;
DEFINE FIELD created_at ON TABLE policy_decision TYPE datetime;
DEFINE INDEX policy_repo_request ON TABLE policy_decision FIELDS repository, request_id;

-- -----------------------------------------------------------------------------
-- Typed relations
-- Relation IDs are deterministic from repository + type + endpoints + discriminator where
-- multiple edges between the same endpoints are meaningful. Application code verifies tenant and
-- repository equality before RELATE.
-- -----------------------------------------------------------------------------

DEFINE TABLE parent_of TYPE RELATION FROM commit TO commit SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE parent_of TYPE record<tenant>;
DEFINE FIELD repository ON TABLE parent_of TYPE record<repository>;
DEFINE FIELD ordinal ON TABLE parent_of TYPE int ASSERT $value IN [0, 1];
DEFINE INDEX parent_edge_unique ON TABLE parent_of FIELDS repository, in, out, ordinal UNIQUE;
DEFINE INDEX parent_by_child ON TABLE parent_of FIELDS repository, out, ordinal;

DEFINE TABLE caused TYPE RELATION FROM span | tool_call TO commit SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE caused TYPE record<tenant>;
DEFINE FIELD repository ON TABLE caused TYPE record<repository>;
DEFINE FIELD capture_quality ON TABLE caused TYPE string
    ASSERT $value IN ["CAPTURED", "IMPORTED", "INFERRED"];
DEFINE INDEX caused_edge_unique ON TABLE caused FIELDS repository, in, out UNIQUE;

DEFINE TABLE owns_workspace TYPE RELATION FROM span | tool_call TO workspace SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE owns_workspace TYPE record<tenant>;
DEFINE FIELD repository ON TABLE owns_workspace TYPE record<repository>;
DEFINE INDEX owns_workspace_unique ON TABLE owns_workspace FIELDS repository, in, out UNIQUE;

DEFINE TABLE based_on TYPE RELATION FROM workspace TO commit SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE based_on TYPE record<tenant>;
DEFINE FIELD repository ON TABLE based_on TYPE record<repository>;
DEFINE INDEX workspace_base_unique ON TABLE based_on FIELDS repository, in, out UNIQUE;

DEFINE TABLE published_as TYPE RELATION FROM workspace TO commit SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE published_as TYPE record<tenant>;
DEFINE FIELD repository ON TABLE published_as TYPE record<repository>;
DEFINE INDEX workspace_publish_unique ON TABLE published_as FIELDS repository, in UNIQUE;

DEFINE TABLE produced TYPE RELATION FROM commit | span | tool_call TO artifact SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE produced TYPE record<tenant>;
DEFINE FIELD repository ON TABLE produced TYPE record<repository>;
DEFINE FIELD role ON TABLE produced TYPE option<string>;
DEFINE INDEX produced_edge_unique ON TABLE produced FIELDS repository, in, out, role UNIQUE;

DEFINE TABLE read_artifact TYPE RELATION FROM span | tool_call TO artifact SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE read_artifact TYPE record<tenant>;
DEFINE FIELD repository ON TABLE read_artifact TYPE record<repository>;
DEFINE FIELD role ON TABLE read_artifact TYPE option<string>;
DEFINE INDEX read_artifact_edge_unique ON TABLE read_artifact FIELDS repository, in, out, role UNIQUE;

DEFINE TABLE derived_from TYPE RELATION FROM artifact TO artifact SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE derived_from TYPE record<tenant>;
DEFINE FIELD repository ON TABLE derived_from TYPE record<repository>;
DEFINE FIELD method ON TABLE derived_from TYPE option<string>;
DEFINE FIELD capture_quality ON TABLE derived_from TYPE string
    ASSERT $value IN ["CAPTURED", "IMPORTED", "INFERRED"];
DEFINE INDEX derived_edge_unique ON TABLE derived_from FIELDS repository, in, out, method UNIQUE;

DEFINE TABLE forked_from TYPE RELATION FROM branch | run TO commit SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE forked_from TYPE record<tenant>;
DEFINE FIELD repository ON TABLE forked_from TYPE record<repository>;
DEFINE INDEX fork_edge_unique ON TABLE forked_from FIELDS repository, in, out UNIQUE;

DEFINE TABLE evaluated TYPE RELATION FROM evaluation TO run | commit | artifact SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE evaluated TYPE record<tenant>;
DEFINE FIELD repository ON TABLE evaluated TYPE record<repository>;
DEFINE INDEX evaluated_edge_unique ON TABLE evaluated FIELDS repository, in, out UNIQUE;

DEFINE TABLE governed TYPE RELATION FROM policy_decision TO run | span | commit | artifact SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE governed TYPE record<tenant>;
DEFINE FIELD repository ON TABLE governed TYPE record<repository>;
DEFINE INDEX governed_edge_unique ON TABLE governed FIELDS repository, in, out UNIQUE;

DEFINE TABLE materializes TYPE RELATION FROM artifact TO commit SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD tenant ON TABLE materializes TYPE record<tenant>;
DEFINE FIELD repository ON TABLE materializes TYPE record<repository>;
DEFINE FIELD path ON TABLE materializes TYPE option<bytes>;
DEFINE INDEX materializes_edge_unique ON TABLE materializes FIELDS repository, in, out, path UNIQUE;

-- Migration bookkeeping. One record per deterministic migration step and repository when a
-- migration has repository-specific work.
DEFINE TABLE migration_receipt TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD migration_id ON TABLE migration_receipt TYPE string;
DEFINE FIELD repository ON TABLE migration_receipt TYPE option<record<repository>>;
DEFINE FIELD input_hash ON TABLE migration_receipt TYPE string;
DEFINE FIELD output_hash ON TABLE migration_receipt TYPE string;
DEFINE FIELD status ON TABLE migration_receipt TYPE string ASSERT $value IN ["STARTED", "VERIFIED", "PUBLISHED"];
DEFINE FIELD started_at ON TABLE migration_receipt TYPE datetime;
DEFINE FIELD finished_at ON TABLE migration_receipt TYPE option<datetime>;
DEFINE INDEX migration_scope_unique ON TABLE migration_receipt FIELDS migration_id, repository UNIQUE;
