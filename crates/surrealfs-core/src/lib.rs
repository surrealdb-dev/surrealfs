//! Embeddable composition root: exclusive store ownership, lifecycle, and repository
//! layout. One process opens one store directory; a second owner is refused by an OS
//! file lock before the engine is even touched.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt;
use surrealfs_kernel::Kernel;
use surrealfs_store::cipher::KeyMaterial;
use surrealfs_store::{Store, StoreConfig, StoreEngine};

/// Build a fresh store config from key material.
///
/// A new one per call because `StoreConfig` owns a `ChunkKey`, which is deliberately not
/// `Clone` — the open path retries, and each attempt needs its own.
fn store_config(key: &Option<KeyMaterial>) -> Result<StoreConfig, SfsError> {
    match key {
        Some(material) => Ok(StoreConfig::with_key(material.key()?)),
        None => Ok(StoreConfig::default()),
    }
}
use surrealfs_types::{RepositoryId, SfsError};

/// How a repository is opened. Mirrors the AgentFS options surface: by id under a base
/// directory, at an explicit path, or ephemeral in memory.
#[derive(Debug, Clone)]
pub struct SfsOptions {
    kind: OptionsKind,
    /// Encrypt chunk bodies with this key. `None` stores content in the clear.
    key: Option<surrealfs_store::cipher::KeyMaterial>,
}

#[derive(Debug, Clone)]
enum OptionsKind {
    Ephemeral { id: String },
    WithId { base: Option<PathBuf>, id: String },
    AtPath { path: PathBuf, id: Option<String> },
}

impl SfsOptions {
    /// In-process memory repository for tests and throwaway sessions.
    pub fn ephemeral() -> Self {
        SfsOptions {
            kind: OptionsKind::Ephemeral {
                id: "ephemeral".to_string(),
            },
            key: None,
        }
    }

    /// Persistent repository under `./.surrealfs/<id>/`.
    pub fn with_id(id: &str) -> Self {
        SfsOptions {
            kind: OptionsKind::WithId {
                base: None,
                id: id.to_string(),
            },
            key: None,
        }
    }

    /// Persistent repository under `<base>/.surrealfs/<id>/`.
    pub fn with_id_in(base: impl Into<PathBuf>, id: &str) -> Self {
        SfsOptions {
            kind: OptionsKind::WithId {
                base: Some(base.into()),
                id: id.to_string(),
            },
            key: None,
        }
    }

    /// Persistent repository at an explicit directory.
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        SfsOptions {
            kind: OptionsKind::AtPath {
                path: path.into(),
                id: None,
            },
            key: None,
        }
    }

    /// Encrypt chunk bodies with a 32-byte key, given as 64 hex characters.
    ///
    /// This encrypts file content and KV values. Paths, file sizes, and commit messages stay in
    /// plaintext — see `surrealfs_store::cipher` for why, and for what that does and does not
    /// protect against.
    pub fn with_key(mut self, hex: &str) -> Result<Self, SfsError> {
        self.key = Some(surrealfs_store::cipher::KeyMaterial::parse(hex)?);
        Ok(self)
    }
}

/// Exclusive owner of one open repository store.
pub struct SfsCore {
    kernel: Kernel,
    /// Held for the lifetime of the core; dropping releases the OS lock.
    _lock: Option<File>,
    dir: Option<PathBuf>,
}

impl SfsCore {
    pub async fn open(options: SfsOptions) -> Result<Self, SfsError> {
        // Validated at construction, so a bad key has already been rejected by now.
        let key = options.key.clone();
        match options.kind {
            OptionsKind::Ephemeral { id } => {
                let repo = RepositoryId::parse(&id)?;
                let store =
                    Arc::new(Store::open_with(StoreEngine::Memory, store_config(&key)?).await?);
                let kernel = Kernel::open(store, repo).await?;
                Ok(SfsCore {
                    kernel,
                    _lock: None,
                    dir: None,
                })
            }
            OptionsKind::WithId { base, id } => {
                let repo = RepositoryId::parse(&id)?;
                let base = match base {
                    Some(base) => base,
                    None => std::env::current_dir()?,
                };
                let dir = base.join(".surrealfs").join(repo.as_str());
                Self::open_dir(dir, repo, key).await
            }
            OptionsKind::AtPath { path, id } => {
                let repo = match id {
                    Some(id) => RepositoryId::parse(&id)?,
                    None => {
                        let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
                            SfsError::InvalidId(format!(
                                "cannot derive repository id from path {}",
                                path.display()
                            ))
                        })?;
                        RepositoryId::parse(name)?
                    }
                };
                Self::open_dir(path, repo, key).await
            }
        }
    }

    async fn open_dir(
        dir: PathBuf,
        repo: RepositoryId,
        key: Option<KeyMaterial>,
    ) -> Result<Self, SfsError> {
        std::fs::create_dir_all(&dir)?;
        let lock = acquire_lock(&dir)?;
        // Our lock guarantees no live second owner. A previous owner's engine lock may
        // still be releasing (drop-based shutdown; awaited shutdown is owned upstream
        // work), so retry within a bounded window.
        let db_path = dir.join("db");
        let deadline = Instant::now() + Duration::from_secs(15);
        let store = loop {
            match Store::open_with(StoreEngine::SurrealKv(db_path.clone()), store_config(&key)?)
                .await
            {
                Ok(store) => break store,
                Err(SfsError::Storage(msg))
                    if msg.contains("locked") && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(other) => return Err(other),
            }
        };
        let kernel = Kernel::open(Arc::new(store), repo).await?;
        Ok(SfsCore {
            kernel,
            _lock: Some(lock),
            dir: Some(dir),
        })
    }

    pub fn kernel(&self) -> &Kernel {
        &self.kernel
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Close the store. Today this is drop-based: the engine flushes asynchronously and
    /// acknowledged commits are already durable per the store's write path. An awaited,
    /// error-reporting shutdown is pinned upstream work (COMPATIBILITY.md).
    pub async fn close(self) -> Result<(), SfsError> {
        drop(self);
        Ok(())
    }
}

fn acquire_lock(dir: &Path) -> Result<File, SfsError> {
    let lock_path = dir.join("surrealfs.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    let acquired = file
        .try_lock_exclusive()
        .map_err(|e| SfsError::Storage(format!("lock {}: {e}", lock_path.display())))?;
    if !acquired {
        return Err(SfsError::StoreLocked(format!(
            "{} is owned by another process",
            lock_path.display()
        )));
    }
    Ok(file)
}
