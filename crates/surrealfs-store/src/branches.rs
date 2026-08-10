//! Branches and named savepoints.
//!
//! Both are constant-time. A branch or a snapshot is a reference to a commit, and a commit
//! already names an immutable state root whose nodes are shared with every other commit that
//! reaches them. Forking a repository copies nothing; it writes one row.

use surrealdb::types::{RecordId, SurrealValue};
use surrealfs_types::{BranchName, CommitId, RepositoryId, SfsError, StateRootId};

use crate::publish::commit_id_of_rid;
use crate::{map_db_err, rid_branch, rid_commit, rid_principal, rid_repo, Store};

#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub head: CommitId,
    pub message: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub name: String,
    pub commit: CommitId,
    pub message: Option<String>,
    pub created_at: String,
}

#[derive(SurrealValue)]
struct BranchRow {
    name: String,
    head: RecordId,
    message: Option<String>,
    updated_at: String,
}

#[derive(SurrealValue)]
struct SnapshotRow {
    name: String,
    commit: RecordId,
    message: Option<String>,
    created_at: String,
}

impl Store {
    /// Create a branch pointing at an existing commit. Fails if the name is taken.
    pub async fn branch_create(
        &self,
        repo: &RepositoryId,
        name: &BranchName,
        at: &CommitId,
        message: Option<String>,
    ) -> Result<(), SfsError> {
        // Confirm the commit exists before binding a name to it.
        self.root_of_commit(repo, at).await?;
        self.db()
            .query(
                "CREATE $rid SET repository = $repo, name = $name, head = $head, \
                 message = $message, created_at = time::now(), updated_at = time::now()",
            )
            .bind(("rid", rid_branch(repo, name)))
            .bind(("repo", rid_repo(repo)))
            .bind(("name", name.as_str().to_string()))
            .bind(("head", rid_commit(repo, at)))
            .bind(("message", message))
            .await
            .map_err(|e| map_db_err("create branch", e))?
            .check()
            .map_err(|_| SfsError::AlreadyExists(format!("branch {name}")))?;
        Ok(())
    }

    pub async fn branches(&self, repo: &RepositoryId) -> Result<Vec<BranchInfo>, SfsError> {
        let rows: Vec<BranchRow> = self
            .db()
            .query(
                "SELECT name, head, message, type::string(updated_at) AS updated_at \
                 FROM branch WHERE repository = $repo ORDER BY name ASC",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("list branches", e))?
            .take(0)
            .map_err(|e| map_db_err("decode branches", e))?;
        rows.into_iter()
            .map(|row| {
                Ok(BranchInfo {
                    name: row.name,
                    head: commit_id_of_rid(&row.head)?,
                    message: row.message,
                    updated_at: row.updated_at,
                })
            })
            .collect()
    }

    /// Bind a name to a commit. Re-using a name rebinds it.
    pub async fn snapshot_create(
        &self,
        repo: &RepositoryId,
        name: &str,
        at: &CommitId,
        message: Option<String>,
    ) -> Result<(), SfsError> {
        self.root_of_commit(repo, at).await?;
        self.db()
            .query(
                "UPSERT $rid CONTENT { repository: $repo, name: $name, commit: $commit, \
                 created_by: $principal, message: $message, created_at: time::now() }",
            )
            .bind(("rid", RecordId::new("snapshot", format!("{repo}/{name}"))))
            .bind(("repo", rid_repo(repo)))
            .bind(("name", name.to_string()))
            .bind(("commit", rid_commit(repo, at)))
            .bind(("principal", rid_principal()))
            .bind(("message", message))
            .await
            .map_err(|e| map_db_err("create snapshot", e))?
            .check()
            .map_err(|e| map_db_err("create snapshot", e))?;
        Ok(())
    }

    pub async fn snapshots(&self, repo: &RepositoryId) -> Result<Vec<SnapshotInfo>, SfsError> {
        let rows: Vec<SnapshotRow> = self
            .db()
            .query(
                "SELECT name, commit, message, type::string(created_at) AS created_at \
                 FROM snapshot WHERE repository = $repo ORDER BY created_at DESC",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("list snapshots", e))?
            .take(0)
            .map_err(|e| map_db_err("decode snapshots", e))?;
        rows.into_iter()
            .map(|row| {
                Ok(SnapshotInfo {
                    name: row.name,
                    commit: commit_id_of_rid(&row.commit)?,
                    message: row.message,
                    created_at: row.created_at,
                })
            })
            .collect()
    }

    /// Resolve a savepoint name to its commit.
    pub async fn snapshot_resolve(
        &self,
        repo: &RepositoryId,
        name: &str,
    ) -> Result<CommitId, SfsError> {
        #[derive(SurrealValue)]
        struct Row {
            commit: RecordId,
        }
        let row: Option<Row> = self
            .db()
            .query("SELECT commit FROM ONLY $rid")
            .bind(("rid", RecordId::new("snapshot", format!("{repo}/{name}"))))
            .await
            .map_err(|e| map_db_err("resolve snapshot", e))?
            .take(0)
            .map_err(|e| map_db_err("decode snapshot", e))?;
        let row = row.ok_or_else(|| SfsError::NotFound(format!("snapshot {name}")))?;
        commit_id_of_rid(&row.commit)
    }

    /// The namespace root and KV map a commit points at, for reverting to it.
    pub async fn root_state_of_commit(
        &self,
        repo: &RepositoryId,
        commit: &CommitId,
    ) -> Result<
        (
            StateRootId,
            surrealfs_types::StateNodeId,
            surrealfs_types::state::KvMap,
        ),
        SfsError,
    > {
        let root = self.root_of_commit(repo, commit).await?;
        let (ns, kv) = self.load_root(repo, &root).await?;
        Ok((root, ns, kv))
    }
}
