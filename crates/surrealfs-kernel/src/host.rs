//! The host boundary: pulling a real directory in, and writing an approved commit back out.
//!
//! These are the only two places SurrealFS touches files outside its own store. Everything
//! between them — the agent's edits, the diff, the review — happens against committed state,
//! so the project on disk is untouched until [`apply`] runs.
//!
//! `apply` refuses to write when the directory has changed underneath it. A change set is
//! computed against a specific base commit, and if the host no longer matches that base then
//! someone edited the files outside SurrealFS; overwriting would destroy work the system
//! never saw. That case is [`SfsError::HostDrift`], not a conflict to resolve automatically.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use surrealfs_content::tree::Entry;
use surrealfs_content::{chunk_bytes, tree};
use surrealfs_types::{CommitId, RepoPath, SfsError};

use crate::{Kernel, Workspace};

/// Directory names skipped by default: build output, dependency trees, and version-control
/// metadata are large, regenerable, and rarely what an agent is asked to reason about.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".git",
    ".surrealfs",
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
];

#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// Path components to skip entirely.
    pub excludes: Vec<String>,
    /// Files larger than this are skipped and reported rather than silently truncated.
    pub max_file_bytes: u64,
    /// Attribute the resulting commit to a tool-call span, so `explain` can name the run that
    /// produced a byte rather than only the fact that an ingest did.
    pub author_span: Option<String>,
    /// Commit message. Defaults to `ingest <path>`.
    pub message: Option<String>,
}

impl Default for IngestOptions {
    fn default() -> Self {
        IngestOptions {
            excludes: DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect(),
            max_file_bytes: 64 * 1024 * 1024,
            author_span: None,
            message: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct IngestReport {
    pub files: usize,
    pub bytes: u64,
    /// Paths deliberately not ingested, with the reason. Reported rather than hidden so the
    /// contents of a repository are never a surprise.
    pub skipped: Vec<(PathBuf, String)>,
    pub commit: Option<CommitId>,
}

/// Read a host directory into the repository and publish it as one commit.
///
/// The result is an ordinary SurrealFS root, which is what makes everything downstream
/// (diff, fork, export) work on ingested projects without a special case for "the base".
pub async fn ingest(
    kernel: &Kernel,
    host_root: &Path,
    opts: &IngestOptions,
) -> Result<IngestReport, SfsError> {
    let mut report = IngestReport::default();
    let mut ws = kernel.workspace().await?;
    if let Some(span) = &opts.author_span {
        ws.attribute_to(span);
    }

    // Files the host reports as having more than one name, keyed by device and inode. The
    // first path seen for an inode is written normally; later ones become hard links to it,
    // so a pnpm-style tree survives a round trip instead of silently inflating into copies.
    let mut by_inode: HashMap<(u64, u64), RepoPath> = HashMap::new();

    let mut stack = vec![(host_root.to_path_buf(), RepoPath::root())];
    while let Some((dir, repo_dir)) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if opts.excludes.contains(&name) {
                report.skipped.push((entry.path(), "excluded".into()));
                continue;
            }
            let repo_path = repo_dir.join(&name)?;
            let meta = entry.metadata().await?;

            if meta.is_symlink() {
                let target = tokio::fs::read_link(entry.path()).await?;
                ws.write_symlink(&repo_path, &target.to_string_lossy())
                    .await?;
                report.files += 1;
            } else if meta.is_dir() {
                ws.mkdir(&repo_path).await?;
                stack.push((entry.path(), repo_path));
            } else if meta.is_file() {
                if meta.len() > opts.max_file_bytes {
                    report
                        .skipped
                        .push((entry.path(), format!("larger than {}", opts.max_file_bytes)));
                    continue;
                }
                #[cfg(unix)]
                let hard_linked = {
                    use std::os::unix::fs::MetadataExt;
                    if meta.nlink() > 1 {
                        match by_inode.get(&(meta.dev(), meta.ino())) {
                            Some(first) => {
                                ws.link(&first.clone(), &repo_path).await?;
                                report.files += 1;
                                true
                            }
                            None => {
                                by_inode.insert((meta.dev(), meta.ino()), repo_path.clone());
                                false
                            }
                        }
                    } else {
                        false
                    }
                };
                #[cfg(not(unix))]
                let hard_linked = false;

                if !hard_linked {
                    let bytes = tokio::fs::read(entry.path()).await?;

                    // Skip files the repository already holds byte for byte.
                    //
                    // Without this, re-ingesting an unchanged tree records a mutation per file and
                    // publishes a commit whose state root is identical to its parent — a history
                    // of no-ops that nobody can read, and O(tree) work to produce it. Comparing
                    // extents is exact rather than heuristic: they are content digests, so equal
                    // extents mean equal bytes.
                    let unchanged = match ws.stat(&repo_path).await? {
                        Some(Entry::File {
                            extents: existing, ..
                        }) => {
                            let (new_extents, _) = chunk_bytes(&bytes);
                            existing == new_extents
                        }
                        _ => false,
                    };
                    if !unchanged {
                        ws.write_file(&repo_path, &bytes).await?;
                        report.files += 1;
                        report.bytes += bytes.len() as u64;
                    }
                }
            } else {
                report.skipped.push((
                    entry.path(),
                    "not a regular file, directory or symlink".into(),
                ));
            }
        }
    }

    if ws.is_dirty() {
        let message = opts
            .message
            .clone()
            .unwrap_or_else(|| format!("ingest {}", host_root.display()));
        let receipt = ws.publish(None, Some(message)).await?;
        report.commit = Some(receipt.commit);
    } else {
        ws.abort("nothing to ingest").await?;
    }
    Ok(report)
}

#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Copy every file this run overwrites or deletes into a backup directory first.
    pub backup_dir: Option<PathBuf>,
    /// Report what would change without touching the host.
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    pub written: Vec<RepoPath>,
    pub removed: Vec<RepoPath>,
    pub backed_up: Vec<RepoPath>,
}

/// Write the difference between two commits into the host directory.
///
/// `base` is the commit the host is expected to still match — normally the commit ingest
/// produced. Every affected path is checked against it before anything is written, so a
/// partially-drifted directory fails before the first byte changes rather than halfway
/// through.
pub async fn apply(
    kernel: &Kernel,
    host_root: &Path,
    base: &CommitId,
    target: &CommitId,
    opts: &ApplyOptions,
) -> Result<ApplyReport, SfsError> {
    let base_root = kernel.store().root_of_commit(kernel.repo(), base).await?;
    let target_root = kernel.store().root_of_commit(kernel.repo(), target).await?;
    let changes = kernel.diff_roots(&base_root, &target_root).await?;

    // Verify the whole change set before mutating anything.
    for change in &changes {
        let path = change.path();
        let host_path = host_path_for(host_root, path)?;
        match change {
            tree::Change::Added(_, entry) => {
                if entry.is_dir() {
                    continue;
                }
                if tokio::fs::try_exists(&host_path).await? {
                    let bytes = tokio::fs::read(&host_path).await?;
                    if !file_matches(&bytes, entry) {
                        return Err(SfsError::HostDrift {
                            path: path.to_string(),
                            detail: "file appeared on the host with different content".into(),
                        });
                    }
                }
            }
            tree::Change::Modified { before, .. } | tree::Change::Removed(_, before) => {
                if before.is_dir() {
                    continue;
                }
                let bytes = match tokio::fs::read(&host_path).await {
                    Ok(bytes) => bytes,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        return Err(SfsError::HostDrift {
                            path: path.to_string(),
                            detail: "file was removed on the host".into(),
                        })
                    }
                    Err(err) => return Err(err.into()),
                };
                if !file_matches(&bytes, before) {
                    return Err(SfsError::HostDrift {
                        path: path.to_string(),
                        detail: "file was edited on the host since the base commit".into(),
                    });
                }
            }
        }
    }

    let mut report = ApplyReport::default();
    if opts.dry_run {
        for change in &changes {
            match change {
                tree::Change::Removed(p, _) => report.removed.push(p.clone()),
                tree::Change::Added(p, e) if !e.is_dir() => report.written.push(p.clone()),
                tree::Change::Modified { path, .. } => report.written.push(path.clone()),
                _ => {}
            }
        }
        return Ok(report);
    }

    // Back up everything this run will disturb, before disturbing any of it.
    if let Some(backup) = &opts.backup_dir {
        for change in &changes {
            let (path, entry) = match change {
                tree::Change::Modified { path, before, .. } => (path, before),
                tree::Change::Removed(path, before) => (path, before),
                tree::Change::Added(..) => continue,
            };
            if entry.is_dir() {
                continue;
            }
            let from = host_path_for(host_root, path)?;
            let to = host_path_for(backup, path)?;
            if let Some(parent) = to.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(&from, &to).await?;
            report.backed_up.push(path.clone());
        }
    }

    // Directories first so files always have somewhere to land; removals last so a directory
    // is only removed once its children are gone.
    for change in &changes {
        if let tree::Change::Added(path, entry) = change {
            if entry.is_dir() {
                tokio::fs::create_dir_all(host_path_for(host_root, path)?).await?;
            }
        }
    }
    for change in &changes {
        let (path, entry) = match change {
            tree::Change::Added(path, entry) => (path, entry),
            tree::Change::Modified { path, after, .. } => (path, after),
            tree::Change::Removed(..) => continue,
        };
        match entry {
            Entry::Dir { .. } => {}
            Entry::File { .. } => {
                let host_path = host_path_for(host_root, path)?;
                if let Some(parent) = host_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                // If another name for this file is already on disk, link to it rather than
                // writing a second copy: that is what makes it the same file again.
                let mut linked = false;
                for sibling in entry.link_group() {
                    if sibling == path {
                        continue;
                    }
                    let sibling_host = host_path_for(host_root, sibling)?;
                    if tokio::fs::try_exists(&sibling_host).await? {
                        let _ = tokio::fs::remove_file(&host_path).await;
                        tokio::fs::hard_link(&sibling_host, &host_path).await?;
                        linked = true;
                        break;
                    }
                }
                if !linked {
                    let bytes = kernel.read_file_at(&target_root, path).await?;
                    tokio::fs::write(&host_path, &bytes).await?;
                }
                report.written.push(path.clone());
            }
            Entry::Symlink { .. } => {
                report.written.push(path.clone());
            }
        }
    }
    let mut removed_dirs: BTreeSet<RepoPath> = BTreeSet::new();
    for change in &changes {
        if let tree::Change::Removed(path, entry) = change {
            let host_path = host_path_for(host_root, path)?;
            if entry.is_dir() {
                removed_dirs.insert(path.clone());
                continue;
            }
            match tokio::fs::remove_file(&host_path).await {
                Ok(()) => report.removed.push(path.clone()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
    }
    // Deepest first, and only if empty: a directory holding unrelated files stays.
    for path in removed_dirs.iter().rev() {
        let host_path = host_path_for(host_root, path)?;
        if tokio::fs::remove_dir(&host_path).await.is_ok() {
            report.removed.push(path.clone());
        }
    }
    Ok(report)
}

/// True when host bytes are exactly the content this entry references.
fn file_matches(bytes: &[u8], entry: &Entry) -> bool {
    match entry {
        Entry::File { size, extents, .. } => {
            if *size != bytes.len() as u64 {
                return false;
            }
            let (host_extents, _) = chunk_bytes(bytes);
            host_extents == *extents
        }
        _ => false,
    }
}

/// Join a repository path onto a host root, refusing anything that would escape it.
fn host_path_for(root: &Path, path: &RepoPath) -> Result<PathBuf, SfsError> {
    let mut out = root.to_path_buf();
    for comp in path.as_str().trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        if comp == ".." || comp == "." || comp.contains('/') {
            return Err(SfsError::InvalidPath(format!(
                "refusing to resolve {path} against a host root"
            )));
        }
        out.push(comp);
    }
    Ok(out)
}

impl Workspace {
    /// Record a symlink. The target is stored verbatim and is not resolved.
    pub async fn write_symlink(&mut self, path: &RepoPath, target: &str) -> Result<(), SfsError> {
        self.write_entry(
            path,
            Entry::Symlink {
                meta: surrealfs_content::tree::Meta::file(),
                target: target.to_string(),
            },
        )
        .await
    }
}
