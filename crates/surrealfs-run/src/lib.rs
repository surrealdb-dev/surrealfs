//! Run a command against a project directory and record exactly what it changed.
//!
//! This is the loop the product exists for: an unmodified tool runs against real files, is
//! confined to the directory it was given, and everything it changed becomes one commit attributed
//! to that run. Afterwards `explain` answers *which command produced this byte* rather than only
//! *when it changed* — the question AgentFS's schema cannot answer, because its `tool_calls` table
//! has no edge to its filesystem tables.
//!
//! **Confinement and attribution are the same mechanism here, not two features.** A command that
//! can write outside the directory has escaped the record, not merely the sandbox: the commit will
//! faithfully describe what happened inside and say nothing about the rest. So confinement is on
//! by default, and turning it off is a decision the caller has to make explicitly and one the
//! report carries afterwards.

use std::path::{Path, PathBuf};

use surrealfs_kernel::host::{self, IngestOptions};
use surrealfs_kernel::Kernel;
use surrealfs_sandbox::Confinement;
use surrealfs_types::{CommitId, SfsError};

/// How to run a command.
pub struct RunOptions {
    /// Confine the command to the working directory. On by default.
    pub confine: bool,
    /// Let the command reach the network. Off by default, and meaningless when `confine` is off.
    pub allow_network: bool,
    /// Extra paths the command may read — a toolchain outside the usual system directories, say.
    pub extra_readable: Vec<PathBuf>,
    /// Commit message. Defaults to the command line.
    pub message: Option<String>,
    /// Paths not to record, on top of the ingest defaults.
    pub excludes: Vec<String>,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            confine: true,
            allow_network: false,
            extra_readable: Vec::new(),
            message: None,
            excludes: Vec::new(),
        }
    }
}

/// What a run did.
#[derive(Debug)]
pub struct RunReport {
    /// The command's exit status, or `None` if a signal killed it.
    pub exit_code: Option<i32>,
    /// The commit recording what changed, or `None` if the command changed nothing.
    pub commit: Option<CommitId>,
    /// Files recorded in that commit.
    pub files: usize,
    /// Whether the command was actually confined. Carried in the report rather than assumed,
    /// because a caller reading a commit later needs to know whether the record is complete.
    pub confined: bool,
    /// The span this run was recorded as, for `explain`.
    pub span_key: String,
}

impl RunReport {
    /// Whether the command reported success.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Run `program` in `dir`, then record what changed as one attributed commit.
///
/// The command's exit status does not decide whether a commit happens. A failed build that wrote
/// half its output changed the directory, and refusing to record that would lose the evidence
/// most worth having — the report carries the exit code so the caller can decide what it means.
pub async fn run_in(
    kernel: &Kernel,
    dir: &Path,
    program: &str,
    args: &[&str],
    opts: &RunOptions,
) -> Result<RunReport, SfsError> {
    let dir = dir
        .canonicalize()
        .map_err(|e| SfsError::InvalidPath(format!("{}: {e}", dir.display())))?;

    let command_line = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");

    // Open the span first, so a command that never returns still leaves a record that it started.
    let span_key = kernel.tool_start("run", Some(command_line.clone())).await?;

    let status = spawn(&dir, program, args, opts).await;
    let (exit_code, spawn_error) = match status {
        Ok(code) => (code, None),
        Err(e) => (None, Some(e.to_string())),
    };

    // Record what changed even when the command failed, then close the span with the outcome.
    let ingest = host::ingest(
        kernel,
        &dir,
        &IngestOptions {
            excludes: {
                let mut e = IngestOptions::default().excludes;
                e.extend(opts.excludes.iter().cloned());
                e
            },
            author_span: Some(span_key.clone()),
            message: Some(
                opts.message
                    .clone()
                    .unwrap_or_else(|| format!("run: {command_line}")),
            ),
            ..IngestOptions::default()
        },
    )
    .await?;

    let outcome = match (&spawn_error, exit_code) {
        (Some(e), _) => Some(format!("could not run: {e}")),
        (None, Some(0)) => None,
        (None, Some(code)) => Some(format!("exited {code}")),
        (None, None) => Some("killed by signal".into()),
    };
    kernel
        .tool_finish(&span_key, Some(command_line), outcome)
        .await?;

    if let Some(e) = spawn_error {
        return Err(SfsError::Io(std::io::Error::other(e)));
    }

    Ok(RunReport {
        exit_code,
        commit: ingest.commit,
        files: ingest.files,
        confined: opts.confine,
        span_key,
    })
}

async fn spawn(
    dir: &Path,
    program: &str,
    args: &[&str],
    opts: &RunOptions,
) -> Result<Option<i32>, SfsError> {
    let mut command = if opts.confine {
        let mut policy = Confinement::for_mount(dir);
        for path in &opts.extra_readable {
            policy = policy.allow_read(path);
        }
        if opts.allow_network {
            policy = policy.allow_network();
        }
        surrealfs_sandbox::confined_command(&policy, program, args)?
    } else {
        let mut c = std::process::Command::new(program);
        c.args(args);
        c
    };
    command.current_dir(dir);

    // The child is a whole process, not a future, and a build can run for minutes. Waiting for it
    // on a blocking thread keeps it off the async runtime's workers, which would otherwise be
    // parked for the duration.
    let status = tokio::task::spawn_blocking(move || command.status())
        .await
        .map_err(|e| SfsError::Io(std::io::Error::other(e.to_string())))?
        .map_err(|e| SfsError::Io(std::io::Error::other(e.to_string())))?;
    Ok(status.code())
}
