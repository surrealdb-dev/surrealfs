//! The semantic kernel: filesystem, KV, workspace, and tool-call rules, implemented once
//! and shared by every surface. All state changes flow through a `Workspace` that stages
//! privately and publishes (or aborts) explicitly — no operation ever mutates published
//! state directly.

pub mod handle;
pub mod host;
pub mod view;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use surrealfs_content::tree::{self, DirNode, Entry, MemNodes, Meta, TreeWriter};
use surrealfs_content::{chunk_bytes, StagedChunk};
use surrealfs_store::{CommitPlan, HeadInfo, Store};
use surrealfs_types::state::{Extent, KvMap, Mutation};
use surrealfs_types::{
    BranchName, ChunkDigest, CommitId, RepoPath, RepositoryId, RequestId, SfsError, StateNodeId,
};

pub use handle::{FileHandle, FileStat, OpenOptions, MAX_OPEN_FILE_BYTES};
pub use surrealfs_store::{
    CommitInfo, CommitReceipt, Provenance, ReceiptOutcome, ToolCallInfo, ToolStats,
};
pub use view::DirEntry;

/// Kernel handle for one open repository.
pub struct Kernel {
    store: Arc<Store>,
    repo: RepositoryId,
    /// The branch this handle reads and publishes to. `on_branch` returns a handle pointed
    /// elsewhere; the run record is shared, so work across branches stays one session.
    branch: BranchName,
    run_key: String,
}

impl Kernel {
    /// Ensure the repository (and this session's run record) exist, then hand out a kernel.
    pub async fn open(store: Arc<Store>, repo: RepositoryId) -> Result<Self, SfsError> {
        store.ensure_repository(&repo).await?;
        let run_key = store.ensure_run(&repo).await?;
        Ok(Kernel {
            store,
            repo,
            branch: BranchName::main(),
            run_key,
        })
    }

    /// A handle on another branch, sharing this session's run record.
    pub fn on_branch(&self, branch: BranchName) -> Kernel {
        Kernel {
            store: self.store.clone(),
            repo: self.repo.clone(),
            branch,
            run_key: self.run_key.clone(),
        }
    }

    pub fn branch(&self) -> &BranchName {
        &self.branch
    }

    pub fn repo(&self) -> &RepositoryId {
        &self.repo
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub async fn head(&self) -> Result<HeadInfo, SfsError> {
        self.store.head(&self.repo, &self.branch).await
    }

    /// Namespace tree root and KV map at the current head.
    pub async fn head_state(&self) -> Result<(HeadInfo, StateNodeId, KvMap), SfsError> {
        let head = self.head().await?;
        let (ns, kv) = self.store.load_root(&self.repo, &head.root).await?;
        Ok((head, ns, kv))
    }

    /// Namespace tree root and KV map of an arbitrary commit (historical read).
    pub async fn state_at(&self, commit: &CommitId) -> Result<(StateNodeId, KvMap), SfsError> {
        let root = self.store.root_of_commit(&self.repo, commit).await?;
        self.store.load_root(&self.repo, &root).await
    }

    /// Open a private workspace based on the current head.
    pub async fn workspace(&self) -> Result<Workspace, SfsError> {
        let (base, namespace_root, kv) = self.head_state().await?;
        let ws_key = self
            .store
            .workspace_open(&self.repo, &self.branch, &base.head)
            .await?;
        Ok(Workspace {
            store: Some(self.store.clone()),
            repo: self.repo.clone(),
            branch: self.branch.clone(),
            base,
            namespace_root,
            kv,
            cache: MemNodes::default(),
            new_nodes: BTreeMap::new(),
            staged: HashMap::new(),
            staged_bytes: 0,
            staged_limit: DEFAULT_STAGED_LIMIT,
            mutations: Vec::new(),
            ws_key,
            author_span: None,
            open: true,
        })
    }

    pub async fn stat_head(&self, path: &RepoPath) -> Result<Option<Entry>, SfsError> {
        let (_, ns, _) = self.head_state().await?;
        view::stat(&self.store, &self.repo, &ns, path).await
    }

    pub async fn list_head(&self, path: &RepoPath) -> Result<Vec<DirEntry>, SfsError> {
        let (_, ns, _) = self.head_state().await?;
        view::list_dir(&self.store, &self.repo, &ns, path).await
    }

    /// Read a file at the current head.
    pub async fn read_head_file(&self, path: &RepoPath) -> Result<Vec<u8>, SfsError> {
        let (_, ns, _) = self.head_state().await?;
        let entry = view::stat(&self.store, &self.repo, &ns, path)
            .await?
            .ok_or_else(|| SfsError::NotFound(path.to_string()))?;
        match entry {
            Entry::File { extents, .. } => {
                let mut out = Vec::new();
                for ext in &extents {
                    let bytes = self.store.fetch_chunk(&self.repo, &ext.chunk).await?;
                    surrealfs_content::verify_chunk(&ext.chunk, &bytes)?;
                    out.extend_from_slice(&bytes);
                }
                Ok(out)
            }
            Entry::Dir { .. } => Err(SfsError::IsADirectory(path.to_string())),
            Entry::Symlink { target, .. } => Ok(target.into_bytes()),
        }
    }

    /// Read one KV value at the current head.
    pub async fn kv_get_head(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, SfsError> {
        let (_, _ns, kv) = self.head_state().await?;
        let Some(digest) = kv.get(&(namespace.to_string(), key.to_string())) else {
            return Ok(None);
        };
        let bytes = self.store.fetch_chunk(&self.repo, digest).await?;
        surrealfs_content::verify_chunk(digest, &bytes)?;
        Ok(Some(bytes))
    }

    /// Read a file at an arbitrary state root (used by apply and historical reads).
    pub async fn read_file_at(
        &self,
        root: &surrealfs_types::StateRootId,
        path: &RepoPath,
    ) -> Result<Vec<u8>, SfsError> {
        let (ns, _) = self.store.load_root(&self.repo, root).await?;
        let entry = view::stat(&self.store, &self.repo, &ns, path)
            .await?
            .ok_or_else(|| SfsError::NotFound(path.to_string()))?;
        match entry {
            Entry::File { extents, .. } => {
                let mut out = Vec::new();
                for ext in &extents {
                    let bytes = self.store.fetch_chunk(&self.repo, &ext.chunk).await?;
                    surrealfs_content::verify_chunk(&ext.chunk, &bytes)?;
                    out.extend_from_slice(&bytes);
                }
                Ok(out)
            }
            Entry::Dir { .. } => Err(SfsError::IsADirectory(path.to_string())),
            Entry::Symlink { target, .. } => Ok(target.into_bytes()),
        }
    }

    /// The repository's first commit, the default base for a diff.
    pub async fn first_commit(&self) -> Result<CommitId, SfsError> {
        self.store.first_commit(&self.repo).await
    }

    /// The commit that was current at a moment.
    ///
    /// Takes a `SystemTime` rather than a string: parsing a user's time reference is a surface
    /// concern, and the kernel should not grow an opinion about grammar.
    pub async fn commit_at_or_before(
        &self,
        at: std::time::SystemTime,
    ) -> Result<CommitId, SfsError> {
        match self.store.commit_at_or_before(&self.repo, at).await? {
            Some(commit) => Ok(commit),
            None => {
                // Name the earliest moment that *does* resolve, so the caller can correct the
                // reference instead of guessing how far back the repository goes.
                let first = self.store.first_commit(&self.repo).await?;
                let when = self
                    .store
                    .committed_at(&self.repo, &first)
                    .await?
                    .unwrap_or_else(|| "an unknown time".to_string());
                Err(SfsError::NotFound(format!(
                    "no commit existed then; this repository begins at {when}"
                )))
            }
        }
    }

    /// When a commit was published.
    pub async fn committed_at(&self, commit: &CommitId) -> Result<Option<String>, SfsError> {
        self.store.committed_at(&self.repo, commit).await
    }

    /// Who changed this path, and why — newest first.
    pub async fn explain(&self, path: &str, limit: usize) -> Result<Vec<Provenance>, SfsError> {
        self.store.explain_path(&self.repo, path, limit).await
    }

    /// Compare two commits directly.
    pub async fn diff_commits(
        &self,
        before: &CommitId,
        after: &CommitId,
    ) -> Result<Vec<tree::Change>, SfsError> {
        let before_root = self.store.root_of_commit(&self.repo, before).await?;
        let after_root = self.store.root_of_commit(&self.repo, after).await?;
        self.diff_roots(&before_root, &after_root).await
    }

    /// Compare two state roots. Unchanged subtrees are skipped by digest.
    pub async fn diff_roots(
        &self,
        before: &surrealfs_types::StateRootId,
        after: &surrealfs_types::StateRootId,
    ) -> Result<Vec<tree::Change>, SfsError> {
        let (before_ns, _) = self.store.load_root(&self.repo, before).await?;
        let (after_ns, _) = self.store.load_root(&self.repo, after).await?;
        let mut cache = MemNodes::default();
        view::load_all_into(&self.store, &self.repo, &before_ns, &mut cache).await?;
        view::load_all_into(&self.store, &self.repo, &after_ns, &mut cache).await?;
        tree::diff(&cache, &before_ns, &after_ns)
    }

    /// Run one operation in its own workspace and publish it as a single commit. On failure
    /// the workspace is aborted, leaving no logical state.
    ///
    /// The operation takes the workspace by value and hands it back with its result, which
    /// keeps the borrow checker out of the caller's way: closures move owned paths and
    /// buffers in rather than borrowing locals across an await.
    pub async fn oneshot<F, Fut>(&self, message: &str, op: F) -> Result<CommitReceipt, SfsError>
    where
        F: FnOnce(Workspace) -> Fut,
        Fut: std::future::Future<Output = (Workspace, Result<(), SfsError>)>,
    {
        let ws = self.workspace().await?;
        let (mut ws, result) = op(ws).await;
        if let Err(err) = result {
            let _ = ws.abort("operation failed").await;
            return Err(err);
        }
        match ws.publish(None, Some(message.to_string())).await {
            Ok(receipt) => Ok(receipt),
            Err(err) => {
                let _ = ws.abort("publish failed").await;
                Err(err)
            }
        }
    }

    /// Record the start of a tool call; the returned span key attributes later commits.
    pub async fn tool_start(
        &self,
        tool_name: &str,
        input_preview: Option<String>,
    ) -> Result<String, SfsError> {
        self.store
            .tool_start(&self.repo, &self.run_key, tool_name, input_preview)
            .await
    }

    pub async fn tool_finish(
        &self,
        span_key: &str,
        output_preview: Option<String>,
        error: Option<String>,
    ) -> Result<(), SfsError> {
        self.store
            .tool_finish(&self.repo, span_key, output_preview, error)
            .await
    }

    pub async fn tool_recent(&self, limit: usize) -> Result<Vec<ToolCallInfo>, SfsError> {
        self.store.tool_recent(&self.repo, limit).await
    }

    /// Per-tool call counts, outcomes, and duration aggregates.
    pub async fn tool_stats(&self) -> Result<Vec<ToolStats>, SfsError> {
        self.store.tool_stats(&self.repo).await
    }

    pub async fn timeline(&self, limit: usize) -> Result<Vec<CommitInfo>, SfsError> {
        self.store.timeline(&self.repo, limit).await
    }
}

/// A private staged view over one base commit. Changes are invisible to every other reader
/// until `publish` succeeds; `abort` (or drop) leaves no logical state.
///
/// The workspace holds the evolving namespace tree root rather than the whole namespace:
/// nodes are pulled in along the routes it touches, and only the nodes it creates are
/// persisted at publish.
pub struct Workspace {
    /// Present while the workspace is open; released on publish/abort so a finished
    /// workspace can never keep the store (and its directory lock) alive.
    store: Option<Arc<Store>>,
    repo: RepositoryId,
    branch: BranchName,
    base: HeadInfo,
    namespace_root: StateNodeId,
    kv: KvMap,
    /// Nodes read from the store plus nodes created here, for the synchronous tree code.
    cache: MemNodes,
    /// Nodes created by this workspace, to be persisted at publish.
    new_nodes: BTreeMap<StateNodeId, DirNode>,
    staged: HashMap<ChunkDigest, Vec<u8>>,
    /// Bytes held in `staged`. Tracked rather than summed on demand because it is consulted on
    /// every write.
    staged_bytes: u64,
    staged_limit: u64,
    mutations: Vec<Mutation>,
    ws_key: String,
    author_span: Option<String>,
    open: bool,
}

/// Ceiling on bytes staged in one unpublished workspace, before publication moves them to the
/// store.
///
/// A workspace that is never published on its own — which is exactly what a long-lived mount is
/// — will otherwise grow without limit and lose everything on a crash. ContextFS declares a cap
/// of this kind and never checks it, and `dofs` enforces a 256 MiB per-file one as `EFBIG`; this
/// is the whole workspace rather than one file, because that is the thing that actually grows.
pub const DEFAULT_STAGED_LIMIT: u64 = 1 << 30;

impl Workspace {
    /// Hold chunk bytes until publication, refusing to grow past the limit.
    ///
    /// Chunks are content-addressed, so re-staging one already held is free and must not be
    /// charged twice — rewriting the same file a hundred times costs one copy, not a hundred.
    /// The whole batch is checked before any of it is staged, so a rejected write leaves the
    /// budget exactly as it found it.
    fn stage_chunks(&mut self, chunks: Vec<StagedChunk>) -> Result<(), SfsError> {
        let incoming: u64 = chunks
            .iter()
            .filter(|c| !self.staged.contains_key(&c.digest))
            .map(|c| c.bytes.len() as u64)
            .sum();
        if self.staged_bytes + incoming > self.staged_limit {
            return Err(SfsError::OverBudget(format!(
                "workspace holds {} staged bytes and this write adds {}, over the {} byte limit; \
                 publish to persist the work or abort to discard it",
                self.staged_bytes, incoming, self.staged_limit
            )));
        }
        for StagedChunk { digest, bytes } in chunks {
            if self.staged.contains_key(&digest) {
                continue;
            }
            self.staged_bytes += bytes.len() as u64;
            self.staged.insert(digest, bytes);
        }
        Ok(())
    }

    /// Bytes currently staged and unpublished.
    pub fn staged_bytes(&self) -> u64 {
        self.staged_bytes
    }

    /// The ceiling those bytes are measured against, so a caller can see pressure coming rather
    /// than only discovering it at the wall.
    pub fn staged_limit(&self) -> u64 {
        self.staged_limit
    }

    /// Raise or lower the ceiling for this workspace.
    pub fn set_staged_limit(&mut self, limit: u64) {
        self.staged_limit = limit;
    }

    fn ensure_open(&self) -> Result<&Arc<Store>, SfsError> {
        match (&self.store, self.open) {
            (Some(store), true) => Ok(store),
            _ => Err(SfsError::WorkspaceClosed {
                status: "CLOSED".into(),
            }),
        }
    }

    pub fn base(&self) -> &HeadInfo {
        &self.base
    }

    pub fn namespace_root(&self) -> &StateNodeId {
        &self.namespace_root
    }

    /// Attribute this workspace's eventual commit to a tool-call span.
    pub fn attribute_to(&mut self, span_key: &str) {
        self.author_span = Some(span_key.to_string());
    }

    /// Pull the directory nodes along `path` into the cache so the tree edit can run.
    async fn prefetch(&mut self, path: &RepoPath) -> Result<(), SfsError> {
        let store = self.ensure_open()?.clone();
        let root = self.namespace_root.clone();
        view::prefetch(&store, &self.repo, &root, path, &mut self.cache).await
    }

    /// Apply a synchronous tree edit, recording the nodes it created.
    fn apply_edit<F>(&mut self, edit: F) -> Result<(), SfsError>
    where
        F: FnOnce(&mut TreeWriter<'_, MemNodes>, &StateNodeId) -> Result<StateNodeId, SfsError>,
    {
        let mut writer = TreeWriter::new(&self.cache);
        let new_root = edit(&mut writer, &self.namespace_root)?;
        let created = writer.into_new_nodes();
        self.cache.insert_all(created.clone());
        self.new_nodes.extend(created);
        self.namespace_root = new_root;
        Ok(())
    }

    async fn entry_at(&mut self, path: &RepoPath) -> Result<Option<Entry>, SfsError> {
        self.prefetch(path).await?;
        tree::get(&self.cache, &self.namespace_root, path)
    }

    /// Insert or replace one entry, rewriting only the nodes on its route.
    pub(crate) async fn write_entry(
        &mut self,
        path: &RepoPath,
        entry: Entry,
    ) -> Result<(), SfsError> {
        self.ensure_open()?;
        if path.is_root() {
            return Err(SfsError::IsADirectory(path.to_string()));
        }
        self.prefetch(path).await?;
        let p = path.clone();
        self.apply_edit(move |w, root| w.insert(root, &p, entry))
    }

    // ---- filesystem ----

    /// Create or overwrite a regular file, creating parent directories implicitly.
    pub async fn write_file(&mut self, path: &RepoPath, bytes: &[u8]) -> Result<(), SfsError> {
        self.ensure_open()?;
        if path.is_root() {
            return Err(SfsError::IsADirectory(path.to_string()));
        }
        if let Some(entry) = self.entry_at(path).await? {
            if entry.is_dir() {
                return Err(SfsError::IsADirectory(path.to_string()));
            }
        }
        let (extents, chunks) = chunk_bytes(bytes);
        self.stage_chunks(chunks)?;

        // Preserve the file's identity across a rewrite: mode and hard-link membership belong
        // to the file, not to this particular write.
        let (meta, links) = match self.entry_at(path).await? {
            Some(existing @ Entry::File { .. }) => {
                (existing.meta(), existing.link_group().to_vec())
            }
            _ => (Meta::file(), Vec::new()),
        };

        let entry = Entry::File {
            meta,
            size: bytes.len() as u64,
            extents: extents.clone(),
            links: links.clone(),
        };
        // Every member of a hard-link group refers to one file, so a write through any of
        // them is a write to all of them.
        let targets = if links.is_empty() {
            vec![path.clone()]
        } else {
            links.clone()
        };
        for target in &targets {
            self.prefetch(target).await?;
        }
        let write_to = targets.clone();
        self.apply_edit(move |w, root| {
            let mut next = root.clone();
            for target in &write_to {
                next = w.insert(&next, target, entry.clone())?;
            }
            Ok(next)
        })?;
        self.mutations.push(Mutation::WriteFile {
            path: path.clone(),
            size: bytes.len() as u64,
            content: extents,
        });
        Ok(())
    }

    /// Create a directory (and missing parents). Existing directories are accepted.
    pub async fn mkdir(&mut self, path: &RepoPath) -> Result<(), SfsError> {
        self.ensure_open()?;
        if path.is_root() {
            return Ok(());
        }
        match self.entry_at(path).await? {
            Some(entry) if entry.is_dir() => return Ok(()),
            Some(_) => return Err(SfsError::AlreadyExists(path.to_string())),
            None => {}
        }
        let entry = Entry::Dir {
            meta: Meta::dir(),
            node: tree::empty_root(),
        };
        let p = path.clone();
        self.apply_edit(move |w, root| w.insert(root, &p, entry))?;
        self.mutations.push(Mutation::MkDir { path: path.clone() });
        Ok(())
    }

    /// Remove a regular file or symlink.
    pub async fn unlink(&mut self, path: &RepoPath) -> Result<(), SfsError> {
        self.ensure_open()?;
        let entry = match self.entry_at(path).await? {
            None => return Err(SfsError::NotFound(path.to_string())),
            Some(entry) if entry.is_dir() => return Err(SfsError::IsADirectory(path.to_string())),
            Some(entry) => entry,
        };

        // Removing one name from a hard-link group leaves the file alive under its remaining
        // names, with their membership corrected. Only removing the last name drops the file.
        let remaining: Vec<RepoPath> = entry
            .link_group()
            .iter()
            .filter(|member| *member != path)
            .cloned()
            .collect();

        let p = path.clone();
        self.apply_edit(move |w, root| w.remove(root, &p))?;
        if !remaining.is_empty() {
            let Entry::File {
                meta,
                size,
                extents,
                ..
            } = entry
            else {
                unreachable!("only files carry link groups")
            };
            self.write_group(&remaining, meta, size, extents).await?;
        }
        self.mutations.push(Mutation::Unlink { path: path.clone() });
        Ok(())
    }

    /// Remove an empty directory.
    pub async fn rmdir(&mut self, path: &RepoPath) -> Result<(), SfsError> {
        let store = self.ensure_open()?.clone();
        let node = match self.entry_at(path).await? {
            None => return Err(SfsError::NotFound(path.to_string())),
            Some(Entry::Dir { node, .. }) => node,
            Some(_) => return Err(SfsError::NotADirectory(path.to_string())),
        };
        // Emptiness is one node read rather than a scan of the namespace.
        let dir = view::load_node(&store, &self.repo, &node, &mut self.cache).await?;
        if !dir.entries.is_empty() {
            return Err(SfsError::DirectoryNotEmpty(path.to_string()));
        }
        let p = path.clone();
        self.apply_edit(move |w, root| w.remove(root, &p))?;
        self.mutations.push(Mutation::RmDir { path: path.clone() });
        Ok(())
    }

    /// Read a file as staged in this workspace.
    pub async fn read_file(&mut self, path: &RepoPath) -> Result<Vec<u8>, SfsError> {
        let store = self.ensure_open()?.clone();
        let entry = self
            .entry_at(path)
            .await?
            .ok_or_else(|| SfsError::NotFound(path.to_string()))?;
        let extents = match entry {
            Entry::File { extents, .. } => extents,
            Entry::Dir { .. } => return Err(SfsError::IsADirectory(path.to_string())),
            Entry::Symlink { target, .. } => return Ok(target.into_bytes()),
        };
        let mut out = Vec::new();
        for ext in &extents {
            let bytes = match self.staged.get(&ext.chunk) {
                Some(bytes) => bytes.clone(),
                None => store.fetch_chunk(&self.repo, &ext.chunk).await?,
            };
            surrealfs_content::verify_chunk(&ext.chunk, &bytes)?;
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }

    pub async fn list_dir(&mut self, path: &RepoPath) -> Result<Vec<DirEntry>, SfsError> {
        let store = self.ensure_open()?.clone();
        let root = self.namespace_root.clone();
        view::prefetch(&store, &self.repo, &root, path, &mut self.cache).await?;
        if let Some(Entry::Dir { node, .. }) = tree::get(&self.cache, &root, path)? {
            view::load_node(&store, &self.repo, &node, &mut self.cache).await?;
        }
        Ok(tree::readdir(&self.cache, &root, path)?
            .into_iter()
            .map(|(name, entry)| DirEntry::from_entry(name, &entry))
            .collect())
    }

    pub async fn stat(&mut self, path: &RepoPath) -> Result<Option<Entry>, SfsError> {
        self.entry_at(path).await
    }

    // ---- key/value ----

    pub fn kv_set(&mut self, namespace: &str, key: &str, value: &[u8]) -> Result<(), SfsError> {
        self.ensure_open()?;
        let digest = ChunkDigest(surrealfs_types::canonical::chunk_digest(value));
        self.stage_chunks(vec![StagedChunk {
            digest: digest.clone(),
            bytes: value.to_vec(),
        }])?;
        self.kv
            .insert((namespace.to_string(), key.to_string()), digest.clone());
        self.mutations.push(Mutation::KvSet {
            namespace: namespace.to_string(),
            key: key.to_string(),
            value: digest,
            value_len: value.len() as u64,
        });
        Ok(())
    }

    pub async fn kv_get(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>, SfsError> {
        let store = self.ensure_open()?;
        let Some(digest) = self.kv.get(&(namespace.to_string(), key.to_string())) else {
            return Ok(None);
        };
        let bytes = match self.staged.get(digest) {
            Some(bytes) => bytes.clone(),
            None => store.fetch_chunk(&self.repo, digest).await?,
        };
        surrealfs_content::verify_chunk(digest, &bytes)?;
        Ok(Some(bytes))
    }

    pub fn kv_delete(&mut self, namespace: &str, key: &str) -> Result<(), SfsError> {
        self.ensure_open()?;
        if self
            .kv
            .remove(&(namespace.to_string(), key.to_string()))
            .is_none()
        {
            return Err(SfsError::NotFound(format!("kv {namespace}/{key}")));
        }
        self.mutations.push(Mutation::KvDelete {
            namespace: namespace.to_string(),
            key: key.to_string(),
        });
        Ok(())
    }

    pub fn kv_list(&self, namespace: &str, prefix: &str) -> Vec<String> {
        self.kv
            .keys()
            .filter(|(ns, key)| ns == namespace && key.starts_with(prefix))
            .map(|(_, key)| key.clone())
            .collect()
    }

    // ---- lifecycle ----

    pub fn is_dirty(&self) -> bool {
        !self.mutations.is_empty()
    }

    /// Publish all staged changes as one atomic commit. On a typed conflict the workspace
    /// stays open so the caller can rebase and retry.
    /// `request_id` may be `None`, in which case one is derived from the base head and the
    /// staged mutations. Re-publishing identical work over the same base is then recognised
    /// as a replay instead of being applied twice.
    pub async fn publish(
        &mut self,
        request_id: Option<&RequestId>,
        message: Option<String>,
    ) -> Result<CommitReceipt, SfsError> {
        let store = self.ensure_open()?.clone();
        let request_id = match request_id {
            Some(id) => id.clone(),
            None => {
                let derived = surrealfs_types::state::command_hash(
                    self.repo.as_str(),
                    self.branch.as_str(),
                    &RequestId::parse("auto").expect("static id"),
                    &self.base.root,
                    &self.mutations,
                );
                RequestId::parse(&format!("auto-{derived}"))?
            }
        };

        // Chunk payloads are staged before, and outside, the publication transaction.
        let staged: Vec<(ChunkDigest, Vec<u8>)> = self
            .staged
            .iter()
            .map(|(d, b)| (d.clone(), b.clone()))
            .collect();
        store.stage_chunks(&self.repo, &staged).await?;

        let plan = CommitPlan {
            repository: self.repo.clone(),
            branch: self.branch.clone(),
            request_id,
            expected_head: self.base.head.clone(),
            base_root: self.base.root.clone(),
            namespace_root: self.namespace_root.clone(),
            new_nodes: self.new_nodes.clone(),
            kv: self.kv.clone(),
            mutations: self.mutations.clone(),
            author_span: self.author_span.clone(),
            workspace: Some(self.ws_key.clone()),
            message,
        };
        let receipt = store.publish(&plan).await?;

        self.open = false;
        self.store = None;
        self.staged.clear();
        self.staged_bytes = 0;
        self.new_nodes.clear();
        Ok(receipt)
    }

    /// Abort: discard every staged change. The workspace record is marked ABORTED; the
    /// staged bytes only ever lived in this process.
    pub async fn abort(&mut self, reason: &str) -> Result<(), SfsError> {
        let store = self.ensure_open()?.clone();
        store.workspace_abort(&self.ws_key, reason).await?;
        self.open = false;
        self.store = None;
        self.staged.clear();
        self.staged_bytes = 0;
        self.new_nodes.clear();
        self.mutations.clear();
        Ok(())
    }
}

/// Branches, savepoints, and reverting.
///
/// Every operation here is constant-time in the size of the repository. A commit already
/// names an immutable state root, and every node beneath it is shared by digest, so binding
/// a new name to an existing commit — or publishing a commit that reuses an old root — moves
/// no content at all.
impl Kernel {
    /// Fork: a new branch at any commit. Copies nothing.
    pub async fn fork(
        &self,
        name: &BranchName,
        at: &CommitId,
        message: Option<String>,
    ) -> Result<Kernel, SfsError> {
        self.store
            .branch_create(&self.repo, name, at, message)
            .await?;
        Ok(self.on_branch(name.clone()))
    }

    pub async fn branches(&self) -> Result<Vec<surrealfs_store::BranchInfo>, SfsError> {
        self.store.branches(&self.repo).await
    }

    /// Bind a name to a commit, defaulting to the current head.
    pub async fn savepoint(
        &self,
        name: &str,
        at: Option<&CommitId>,
        message: Option<String>,
    ) -> Result<CommitId, SfsError> {
        let commit = match at {
            Some(commit) => commit.clone(),
            None => self.head().await?.head,
        };
        self.store
            .snapshot_create(&self.repo, name, &commit, message)
            .await?;
        Ok(commit)
    }

    pub async fn savepoints(&self) -> Result<Vec<surrealfs_store::SnapshotInfo>, SfsError> {
        self.store.snapshots(&self.repo).await
    }

    pub async fn resolve_savepoint(&self, name: &str) -> Result<CommitId, SfsError> {
        self.store.snapshot_resolve(&self.repo, name).await
    }

    /// Return the branch to an earlier commit's state by publishing a *new* commit that
    /// reuses that commit's root.
    ///
    /// History is preserved rather than rewritten: the harmful commits remain, explainable,
    /// with a compensating commit after them. Because the target root and all its nodes
    /// already exist, this writes commit metadata and nothing else, whatever the repository's
    /// size.
    pub async fn revert_to(
        &self,
        target: &CommitId,
        message: Option<String>,
    ) -> Result<CommitReceipt, SfsError> {
        let head = self.head().await?;
        let (_, namespace_root, kv) = self.store.root_state_of_commit(&self.repo, target).await?;

        // Describe the reversal in the mutation log so `explain` covers it like any other
        // change, rather than a path appearing to change with no recorded cause.
        let changes = self.diff_commits(&head.head, target).await?;
        let mutations = changes
            .iter()
            .filter_map(|change| match change {
                tree::Change::Added(path, entry)
                | tree::Change::Modified {
                    path, after: entry, ..
                } => match entry {
                    Entry::File { size, extents, .. } => Some(Mutation::WriteFile {
                        path: path.clone(),
                        size: *size,
                        content: extents.clone(),
                    }),
                    Entry::Dir { .. } => Some(Mutation::MkDir { path: path.clone() }),
                    Entry::Symlink { .. } => None,
                },
                tree::Change::Removed(path, entry) => Some(if entry.is_dir() {
                    Mutation::RmDir { path: path.clone() }
                } else {
                    Mutation::Unlink { path: path.clone() }
                }),
            })
            .collect::<Vec<_>>();

        if mutations.is_empty() {
            return Err(SfsError::NotFound(format!(
                "reverting to {target} would change nothing"
            )));
        }

        let plan = CommitPlan {
            repository: self.repo.clone(),
            branch: self.branch.clone(),
            request_id: RequestId::parse(&format!(
                "revert-{}-{}",
                &head.head.as_str()[..16],
                &target.as_str()[..16]
            ))?,
            expected_head: head.head.clone(),
            base_root: head.root.clone(),
            // Every node is already stored; reverting introduces none.
            namespace_root,
            new_nodes: BTreeMap::new(),
            kv,
            mutations,
            author_span: None,
            workspace: None,
            message: Some(message.unwrap_or_else(|| format!("revert to {target}"))),
        };
        self.store.publish(&plan).await
    }
}

/// Filesystem operations beyond create and remove.
impl Workspace {
    /// Move a file, symlink, or whole directory.
    ///
    /// Identity is the path, so the tree records a removal and an insertion; the relationship
    /// between them lives in a `Rename` mutation. Moving a directory therefore costs one tree
    /// edit rather than a rewrite of the subtree, because the subtree node is reattached by
    /// digest.
    pub async fn rename(&mut self, from: &RepoPath, to: &RepoPath) -> Result<(), SfsError> {
        self.ensure_open()?;
        if from.is_root() || to.is_root() {
            return Err(SfsError::InvalidPath("cannot rename the root".into()));
        }
        if from == to {
            return Ok(());
        }
        // Moving a directory inside itself would detach the subtree from the tree entirely.
        if to.starts_with(from) {
            return Err(SfsError::InvalidPath(format!(
                "cannot move {from} inside itself"
            )));
        }

        let entry = self
            .entry_at(from)
            .await?
            .ok_or_else(|| SfsError::NotFound(from.to_string()))?;

        match self.entry_at(to).await? {
            // Replacing a directory is only safe when it is empty, matching POSIX.
            Some(Entry::Dir { node, .. }) => {
                let store = self.ensure_open()?.clone();
                let dir = view::load_node(&store, &self.repo, &node, &mut self.cache).await?;
                if !dir.entries.is_empty() {
                    return Err(SfsError::DirectoryNotEmpty(to.to_string()));
                }
                if !entry.is_dir() {
                    return Err(SfsError::IsADirectory(to.to_string()));
                }
            }
            Some(_) if entry.is_dir() => return Err(SfsError::NotADirectory(to.to_string())),
            _ => {}
        }

        self.prefetch(to).await?;
        let (src, dst) = (from.clone(), to.clone());
        let moved = entry.clone();
        self.apply_edit(move |w, root| {
            let after_insert = w.insert(root, &dst, moved)?;
            w.remove(&after_insert, &src)
        })?;
        self.mutations.push(Mutation::Rename {
            from: from.clone(),
            to: to.clone(),
        });
        Ok(())
    }

    /// Copy a file or symlink. Content is shared by digest, so this moves no bytes.
    pub async fn copy(&mut self, from: &RepoPath, to: &RepoPath) -> Result<(), SfsError> {
        self.ensure_open()?;
        let entry = self
            .entry_at(from)
            .await?
            .ok_or_else(|| SfsError::NotFound(from.to_string()))?;
        if entry.is_dir() {
            return Err(SfsError::IsADirectory(from.to_string()));
        }
        if let Some(existing) = self.entry_at(to).await? {
            if existing.is_dir() {
                return Err(SfsError::IsADirectory(to.to_string()));
            }
        }
        // Recorded as an ordinary write: the destination's content is what matters, and the
        // extents already reference stored chunks.
        match &entry {
            Entry::File { size, extents, .. } => self.mutations.push(Mutation::WriteFile {
                path: to.clone(),
                size: *size,
                content: extents.clone(),
            }),
            Entry::Symlink { target, .. } => self.mutations.push(Mutation::Symlink {
                path: to.clone(),
                target: target.clone(),
            }),
            Entry::Dir { .. } => unreachable!("directories rejected above"),
        }
        // A copy is a new, independent file: it shares content by digest but never joins the
        // source's hard-link group.
        let independent = match entry {
            Entry::File {
                meta,
                size,
                extents,
                ..
            } => Entry::File {
                meta,
                size,
                extents,
                links: Vec::new(),
            },
            other => other,
        };
        self.write_entry(to, independent).await
    }

    /// Create a symlink. The target is stored verbatim and never resolved at write time.
    pub async fn symlink(&mut self, path: &RepoPath, target: &str) -> Result<(), SfsError> {
        self.ensure_open()?;
        if self.entry_at(path).await?.is_some() {
            return Err(SfsError::AlreadyExists(path.to_string()));
        }
        self.write_symlink(path, target).await?;
        self.mutations.push(Mutation::Symlink {
            path: path.clone(),
            target: target.to_string(),
        });
        Ok(())
    }

    /// Read a symlink's target without following it.
    pub async fn readlink(&mut self, path: &RepoPath) -> Result<String, SfsError> {
        match self.entry_at(path).await? {
            Some(Entry::Symlink { target, .. }) => Ok(target),
            Some(_) => Err(SfsError::InvalidPath(format!("{path} is not a symlink"))),
            None => Err(SfsError::NotFound(path.to_string())),
        }
    }

    /// Change mode, owner, or group. Content and its digest are untouched, so only the
    /// entry's own node changes.
    pub async fn set_meta(
        &mut self,
        path: &RepoPath,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<(), SfsError> {
        self.ensure_open()?;
        let entry = self
            .entry_at(path)
            .await?
            .ok_or_else(|| SfsError::NotFound(path.to_string()))?;
        let current = entry.meta();
        let next = Meta {
            mode: mode.unwrap_or(current.mode),
            uid: uid.unwrap_or(current.uid),
            gid: gid.unwrap_or(current.gid),
        };
        if next == current {
            return Ok(());
        }
        let updated = match entry {
            Entry::Dir { node, .. } => Entry::Dir { meta: next, node },
            Entry::File {
                size,
                extents,
                links,
                ..
            } => Entry::File {
                meta: next,
                size,
                extents,
                links,
            },
            Entry::Symlink { target, .. } => Entry::Symlink { meta: next, target },
        };
        self.write_entry(path, updated).await?;
        self.mutations.push(Mutation::SetMeta {
            path: path.clone(),
            mode: next.mode,
            uid: next.uid,
            gid: next.gid,
        });
        Ok(())
    }
}

/// Hard links.
///
/// A hard-link group is recorded as content — the sorted list of member paths, stored on every
/// member's entry — rather than as an allocated inode number. That keeps the state root a pure
/// function of logical state: two files holding identical bytes are not linked unless the group
/// says so, and reaching the same set of links by a different route yields the same digest.
impl Workspace {
    /// Point a second path at an existing file. Both names then refer to one file: a write
    /// through either is visible through both, and removing one leaves the other intact.
    pub async fn link(&mut self, existing: &RepoPath, new: &RepoPath) -> Result<(), SfsError> {
        self.ensure_open()?;
        let entry = self
            .entry_at(existing)
            .await?
            .ok_or_else(|| SfsError::NotFound(existing.to_string()))?;
        // Directories are excluded for the same reason POSIX excludes them: a link into a
        // directory's own subtree would make the namespace cyclic.
        let (meta, size, extents) = match &entry {
            Entry::File {
                meta,
                size,
                extents,
                ..
            } => (*meta, *size, extents.clone()),
            Entry::Dir { .. } => return Err(SfsError::IsADirectory(existing.to_string())),
            Entry::Symlink { .. } => {
                return Err(SfsError::InvalidPath(format!(
                    "{existing} is a symlink; link its target instead"
                )))
            }
        };
        if self.entry_at(new).await?.is_some() {
            return Err(SfsError::AlreadyExists(new.to_string()));
        }

        let mut group = entry.link_group().to_vec();
        if group.is_empty() {
            group.push(existing.clone());
        }
        group.push(new.clone());
        group.sort();
        group.dedup();

        self.write_group(&group, meta, size, extents).await?;
        self.mutations.push(Mutation::Link {
            from: existing.clone(),
            to: new.clone(),
        });
        Ok(())
    }

    /// Write one entry to every member of a link group.
    async fn write_group(
        &mut self,
        group: &[RepoPath],
        meta: Meta,
        size: u64,
        extents: Vec<Extent>,
    ) -> Result<(), SfsError> {
        let entry = Entry::File {
            meta,
            size,
            extents,
            // A single remaining member is an ordinary file again, not a group of one.
            links: if group.len() > 1 {
                group.to_vec()
            } else {
                Vec::new()
            },
        };
        for path in group {
            self.prefetch(path).await?;
        }
        let targets = group.to_vec();
        self.apply_edit(move |w, root| {
            let mut next = root.clone();
            for path in &targets {
                next = w.insert(&next, path, entry.clone())?;
            }
            Ok(next)
        })
    }

    /// How many names refer to this file.
    pub async fn link_count(&mut self, path: &RepoPath) -> Result<usize, SfsError> {
        Ok(self
            .entry_at(path)
            .await?
            .ok_or_else(|| SfsError::NotFound(path.to_string()))?
            .link_count())
    }
}
