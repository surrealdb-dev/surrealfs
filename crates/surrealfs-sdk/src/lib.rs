//! The public SurrealFS Rust SDK — the only supported client-language SDK.
//!
//! ```no_run
//! use surrealfs_sdk::{Surrealfs, SfsOptions};
//!
//! # async fn example() -> Result<(), surrealfs_sdk::SfsError> {
//! let sfs = Surrealfs::open(SfsOptions::with_id("my-agent")).await?;
//!
//! // Key-value operations
//! sfs.kv().set("user:preferences", b"{\"theme\":\"dark\"}").await?;
//! let prefs = sfs.kv().get("user:preferences").await?;
//!
//! // Filesystem operations
//! sfs.fs().write_file("/output/report.md", b"# Report").await?;
//! let files = sfs.fs().readdir("/output").await?;
//!
//! // Tool call tracking
//! sfs.tools().record("web_search", Some("{\"query\":\"AI\"}"), Some("3 results")).await?;
//! # Ok(())
//! # }
//! ```
//!
//! One-shot `fs`/`kv` calls each publish one atomic commit. For multi-operation atomic
//! publication, open an explicit [`Workspace`] via [`Surrealfs::workspace`].

use std::sync::Arc;

pub use surrealfs_content::tree::Entry;
pub use surrealfs_core::SfsOptions;
use surrealfs_kernel::{CommitInfo, CommitReceipt, DirEntry, ToolCallInfo, Workspace};
pub use surrealfs_types::state::{self, InodeMeta};
pub use surrealfs_types::{CommitId, RepoPath, SfsError, StateRootId};

use surrealfs_core::SfsCore;

/// Cloneable handle over one embedded repository.
#[derive(Clone)]
pub struct Surrealfs {
    core: Arc<SfsCore>,
}

impl Surrealfs {
    pub async fn open(options: SfsOptions) -> Result<Self, SfsError> {
        Ok(Surrealfs {
            core: Arc::new(SfsCore::open(options).await?),
        })
    }

    pub fn fs(&self) -> Fs<'_> {
        Fs { sfs: self }
    }

    pub fn kv(&self) -> Kv<'_> {
        Kv { sfs: self }
    }

    pub fn tools(&self) -> Tools<'_> {
        Tools { sfs: self }
    }

    /// The semantic kernel behind this handle.
    ///
    /// Surfaces that need kernel-level operations (ingest, apply, diff, explain, MCP) go
    /// through here rather than reaching past the SDK into the store.
    pub fn kernel(&self) -> &surrealfs_kernel::Kernel {
        self.core.kernel()
    }

    /// Open an explicit private workspace for multi-operation atomic publication.
    pub async fn workspace(&self) -> Result<Workspace, SfsError> {
        self.core.kernel().workspace().await
    }

    /// Recent commits, newest first.
    pub async fn timeline(&self, limit: usize) -> Result<Vec<CommitInfo>, SfsError> {
        self.core.kernel().timeline(limit).await
    }

    /// Current head commit and verified state root.
    pub async fn head(&self) -> Result<(CommitId, StateRootId), SfsError> {
        // load_root re-derives the root from the nodes it read and fails on any mismatch,
        // so reaching here is itself the verification.
        let (head, _ns, _kv) = self.core.kernel().head_state().await?;
        Ok((head.head, head.root))
    }

    /// Close this handle's store. Fails if other clones are still alive.
    pub async fn close(self) -> Result<(), SfsError> {
        match Arc::try_unwrap(self.core) {
            Ok(core) => core.close().await,
            Err(_) => Err(SfsError::Storage(
                "cannot close: other Surrealfs clones are still alive".into(),
            )),
        }
    }
}

/// One-shot filesystem operations; every mutation is one atomic commit.
pub struct Fs<'a> {
    sfs: &'a Surrealfs,
}

impl Fs<'_> {
    pub async fn write_file(&self, path: &str, bytes: &[u8]) -> Result<CommitReceipt, SfsError> {
        let path = RepoPath::parse(path)?;
        let bytes = bytes.to_vec();
        let message = format!("fs write {path}");
        self.sfs
            .core
            .kernel()
            .oneshot(&message, move |mut ws| async move {
                let r = ws.write_file(&path, &bytes).await;
                (ws, r)
            })
            .await
    }

    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>, SfsError> {
        let path = RepoPath::parse(path)?;
        self.sfs.core.kernel().read_head_file(&path).await
    }

    pub async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, SfsError> {
        let path = RepoPath::parse(path)?;
        self.sfs.core.kernel().list_head(&path).await
    }

    pub async fn stat(&self, path: &str) -> Result<Option<Entry>, SfsError> {
        let path = RepoPath::parse(path)?;
        self.sfs.core.kernel().stat_head(&path).await
    }

    pub async fn mkdir(&self, path: &str) -> Result<CommitReceipt, SfsError> {
        let path = RepoPath::parse(path)?;
        let message = format!("fs mkdir {path}");
        self.sfs
            .core
            .kernel()
            .oneshot(&message, move |mut ws| async move {
                let r = ws.mkdir(&path).await;
                (ws, r)
            })
            .await
    }

    pub async fn remove_file(&self, path: &str) -> Result<CommitReceipt, SfsError> {
        let path = RepoPath::parse(path)?;
        let message = format!("fs rm {path}");
        self.sfs
            .core
            .kernel()
            .oneshot(&message, move |mut ws| async move {
                let r = ws.unlink(&path).await;
                (ws, r)
            })
            .await
    }

    /// Move a file, symlink, or directory. Recorded as a rename, not a delete plus an add.
    pub async fn rename(&self, from: &str, to: &str) -> Result<CommitReceipt, SfsError> {
        let from = RepoPath::parse(from)?;
        let to = RepoPath::parse(to)?;
        let message = format!("fs rename {from} -> {to}");
        self.sfs
            .core
            .kernel()
            .oneshot(&message, move |mut ws| async move {
                let r = ws.rename(&from, &to).await;
                (ws, r)
            })
            .await
    }

    /// Copy a file or symlink. Content is shared by digest, so no bytes are duplicated.
    pub async fn copy(&self, from: &str, to: &str) -> Result<CommitReceipt, SfsError> {
        let from = RepoPath::parse(from)?;
        let to = RepoPath::parse(to)?;
        let message = format!("fs copy {from} -> {to}");
        self.sfs
            .core
            .kernel()
            .oneshot(&message, move |mut ws| async move {
                let r = ws.copy(&from, &to).await;
                (ws, r)
            })
            .await
    }

    pub async fn symlink(&self, path: &str, target: &str) -> Result<CommitReceipt, SfsError> {
        let path = RepoPath::parse(path)?;
        let target = target.to_string();
        let message = format!("fs symlink {path}");
        self.sfs
            .core
            .kernel()
            .oneshot(&message, move |mut ws| async move {
                let r = ws.symlink(&path, &target).await;
                (ws, r)
            })
            .await
    }

    /// A symlink's target, unresolved.
    pub async fn readlink(&self, path: &str) -> Result<String, SfsError> {
        let path = RepoPath::parse(path)?;
        match self.sfs.core.kernel().stat_head(&path).await? {
            Some(Entry::Symlink { target, .. }) => Ok(target),
            Some(_) => Err(SfsError::InvalidPath(format!("{path} is not a symlink"))),
            None => Err(SfsError::NotFound(path.to_string())),
        }
    }

    /// Change mode, owner, or group. `None` leaves a field alone.
    pub async fn set_meta(
        &self,
        path: &str,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<CommitReceipt, SfsError> {
        let path = RepoPath::parse(path)?;
        let message = format!("fs setmeta {path}");
        self.sfs
            .core
            .kernel()
            .oneshot(&message, move |mut ws| async move {
                let r = ws.set_meta(&path, mode, uid, gid).await;
                (ws, r)
            })
            .await
    }

    pub async fn remove_dir(&self, path: &str) -> Result<CommitReceipt, SfsError> {
        let path = RepoPath::parse(path)?;
        let message = format!("fs rmdir {path}");
        self.sfs
            .core
            .kernel()
            .oneshot(&message, move |mut ws| async move {
                let r = ws.rmdir(&path).await;
                (ws, r)
            })
            .await
    }
}

/// Default KV namespace for the AgentFS-style flat key API.
const KV_NAMESPACE: &str = "default";

/// One-shot key-value operations; every mutation is one atomic commit.
pub struct Kv<'a> {
    sfs: &'a Surrealfs,
}

impl Kv<'_> {
    pub async fn set(&self, key: &str, value: &[u8]) -> Result<CommitReceipt, SfsError> {
        let message = format!("kv set {key}");
        let key = key.to_string();
        let value = value.to_vec();
        self.sfs
            .core
            .kernel()
            .oneshot(&message, move |mut ws| async move {
                let r = ws.kv_set(KV_NAMESPACE, &key, &value);
                (ws, r)
            })
            .await
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SfsError> {
        self.sfs.core.kernel().kv_get_head(KV_NAMESPACE, key).await
    }

    pub async fn delete(&self, key: &str) -> Result<CommitReceipt, SfsError> {
        let message = format!("kv delete {key}");
        let key = key.to_string();
        self.sfs
            .core
            .kernel()
            .oneshot(&message, move |mut ws| async move {
                let r = ws.kv_delete(KV_NAMESPACE, &key);
                (ws, r)
            })
            .await
    }

    /// Keys with the given prefix, in deterministic order.
    pub async fn keys(&self, prefix: &str) -> Result<Vec<String>, SfsError> {
        let (_, _ns, kv) = self.sfs.core.kernel().head_state().await?;
        Ok(kv
            .keys()
            .filter(|(ns, key)| ns == KV_NAMESPACE && key.starts_with(prefix))
            .map(|(_, key)| key.clone())
            .collect())
    }
}

/// Tool-call tracking.
pub struct Tools<'a> {
    sfs: &'a Surrealfs,
}

/// An in-flight tool call; finish it with [`success`](ToolHandle::success) or
/// [`error`](ToolHandle::error), and pass it to [`Workspace::attribute_to`] to attribute
/// a published commit to this call.
pub struct ToolHandle {
    span_key: String,
}

impl ToolHandle {
    pub fn span_key(&self) -> &str {
        &self.span_key
    }
}

impl Tools<'_> {
    /// Record a completed tool call in one step.
    pub async fn record(
        &self,
        name: &str,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<(), SfsError> {
        let handle = self.start(name, input).await?;
        self.success(&handle, output).await
    }

    pub async fn start(&self, name: &str, input: Option<&str>) -> Result<ToolHandle, SfsError> {
        let span_key = self
            .sfs
            .core
            .kernel()
            .tool_start(name, input.map(|s| s.to_string()))
            .await?;
        Ok(ToolHandle { span_key })
    }

    pub async fn success(&self, handle: &ToolHandle, output: Option<&str>) -> Result<(), SfsError> {
        self.sfs
            .core
            .kernel()
            .tool_finish(&handle.span_key, output.map(|s| s.to_string()), None)
            .await
    }

    pub async fn error(&self, handle: &ToolHandle, message: &str) -> Result<(), SfsError> {
        self.sfs
            .core
            .kernel()
            .tool_finish(&handle.span_key, None, Some(message.to_string()))
            .await
    }

    pub async fn recent(&self, limit: usize) -> Result<Vec<ToolCallInfo>, SfsError> {
        self.sfs.core.kernel().tool_recent(limit).await
    }
}
