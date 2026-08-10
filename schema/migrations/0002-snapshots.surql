-- SurrealFS migration 0002: named savepoints.
--
-- A snapshot is a name bound to a commit. It costs one row regardless of repository size,
-- because the commit already references an immutable state root and every node beneath it is
-- shared. "Take a snapshot" is therefore a constant-time operation rather than a copy.

DEFINE TABLE snapshot TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
DEFINE FIELD repository ON TABLE snapshot TYPE record<repository>;
DEFINE FIELD name ON TABLE snapshot TYPE string;
DEFINE FIELD commit ON TABLE snapshot TYPE record<commit>;
DEFINE FIELD created_by ON TABLE snapshot TYPE record<principal>;
DEFINE FIELD message ON TABLE snapshot TYPE option<string>;
DEFINE FIELD created_at ON TABLE snapshot TYPE datetime;
DEFINE INDEX snapshot_repo_name_unique ON TABLE snapshot FIELDS repository, name UNIQUE;
DEFINE INDEX snapshot_repo_commit ON TABLE snapshot FIELDS repository, commit;

-- Branches gain a description so a fork can record why it was taken. The branch table itself
-- already exists; only the optional column is new.
DEFINE FIELD message ON TABLE branch TYPE option<string>;
DEFINE INDEX branch_repo_name_unique ON TABLE branch FIELDS repository, name UNIQUE;

-- The commit's repository-wide sequence, denormalised onto the mutation. Ordering a query by
-- a field reached through a record link does not sort reliably, and provenance is read
-- newest-first often enough to be worth one extra column.
DEFINE FIELD domain_sequence ON TABLE commit_mutation TYPE int ASSERT $value >= 0;
DEFINE INDEX mutation_repo_path_sequence
    ON TABLE commit_mutation FIELDS repository, path, domain_sequence;
