//! `surrealfs` CLI: a thin wrapper over the Rust SDK and kernel; it never touches the store
//! directly.
//!
//! The shape of the agent loop is `init` a project, let an agent work through the SDK or MCP,
//! `diff` to review, then `apply` to reconcile the real directory. Nothing outside `apply`
//! writes to the project on disk.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use surrealfs_content::tree::Change;
use surrealfs_kernel::host::{self, ApplyOptions, IngestOptions};
use surrealfs_sdk::{SfsOptions, Surrealfs};
use surrealfs_types::{BranchName, CommitId};

#[derive(Parser)]
#[command(
    name = "surrealfs",
    version,
    about = "SurrealFS: transactional agent filesystem"
)]
struct Cli {
    /// Repository id (directory .surrealfs/<id>/ under the base directory)
    #[arg(long, global = true, default_value = "default")]
    repo: String,

    /// Base directory holding .surrealfs/ (defaults to the current directory)
    #[arg(long, global = true)]
    base: Option<std::path::PathBuf>,

    /// Encrypt content with this 32-byte key, as 64 hex characters
    ///
    /// Prefer the SURREALFS_KEY environment variable: an argument is visible to every user on
    /// the machine through `ps` and is kept in shell history. This encrypts file content and
    /// KV values; paths, file sizes, and commit messages stay readable. An archive exported
    /// from an encrypted repository is written in plaintext and needs the key to produce.
    #[arg(long, global = true)]
    key: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a repository, optionally ingesting a project directory into it
    Init {
        /// Project directory to read in. Without it, the repository starts empty.
        dir: Option<std::path::PathBuf>,
    },
    /// Show what changed between two commits (defaults to the first and current commit)
    Diff {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Write an approved commit back to a project directory
    Apply {
        /// Project directory to reconcile
        dir: std::path::PathBuf,
        /// Commit the directory currently matches
        #[arg(long)]
        from: String,
        /// Commit to write (defaults to the current head)
        #[arg(long)]
        to: Option<String>,
        /// Report what would change without touching anything
        #[arg(long)]
        dry_run: bool,
        /// Copy every overwritten or deleted file here first
        #[arg(long)]
        backup: Option<std::path::PathBuf>,
    },
    /// Which tool calls changed a path, newest first
    Explain {
        path: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Named savepoints
    #[command(subcommand)]
    Savepoint(SavepointCommand),
    /// Branches
    #[command(subcommand)]
    Branch(BranchCommand),
    /// Return the branch to an earlier commit by publishing a compensating commit
    Revert {
        /// Commit id, or a savepoint name
        target: String,
        #[arg(long)]
        message: Option<String>,
    },
    /// Write the whole session to a portable archive
    Export {
        /// Archive file to create
        file: std::path::PathBuf,
    },
    /// Read a session archive into this repository, verifying every root
    Import {
        /// Archive file to read
        file: std::path::PathBuf,
    },
    /// Reclaim content and tree nodes that nothing references
    Gc {
        /// Keep objects younger than this many seconds, whatever their reachability
        #[arg(long, default_value_t = surrealfs_store::DEFAULT_GRACE_SECONDS)]
        grace: i64,
    },
    /// Run a command against a directory and record exactly what it changed
    ///
    /// The command is confined to the directory, so anything it did is in the commit and
    /// nothing it did is outside it. Use `--` before the command.
    Run {
        /// Directory the command runs in and is confined to
        #[arg(long, default_value = ".")]
        dir: std::path::PathBuf,
        /// Run without confinement. The resulting commit may be an incomplete record.
        #[arg(long)]
        no_sandbox: bool,
        /// Let the command reach the network
        #[arg(long)]
        allow_network: bool,
        /// Additional paths the command may read
        #[arg(long = "allow-read", value_name = "PATH")]
        allow_read: Vec<std::path::PathBuf>,
        /// Commit message; defaults to the command line
        #[arg(long, short)]
        message: Option<String>,
        /// The command and its arguments
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Show which schema migrations have run, and which an upgrade would apply
    ///
    /// Opening a repository is what normally applies migrations, so this opens without
    /// applying anything — the point is to see what *would* happen first.
    Migrate {
        /// Apply pending migrations rather than only reporting them
        #[arg(long)]
        apply: bool,
    },
    /// Print a shell completion script
    ///
    /// e.g. `surrealfs completions zsh > "${fpath[1]}/_surrealfs"`
    Completions {
        /// bash, zsh, fish, elvish, or powershell
        shell: clap_complete::Shell,
    },
    /// Serve MCP on stdio
    Mcp,
    /// Filesystem operations
    #[command(subcommand)]
    Fs(FsCommand),
    /// Key-value operations
    #[command(subcommand)]
    Kv(KvCommand),
    /// Show recent commits
    Timeline {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Tool-call records
    #[command(subcommand)]
    Tools(ToolsCommand),
}

#[derive(Subcommand)]
enum SavepointCommand {
    /// Name the current head (or a given commit)
    Create {
        name: String,
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        message: Option<String>,
    },
    /// List savepoints
    List,
}

#[derive(Subcommand)]
enum BranchCommand {
    /// Fork a new branch at a commit or savepoint. Copies nothing.
    Create {
        name: String,
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        message: Option<String>,
    },
    /// List branches
    List,
}

#[derive(Subcommand)]
enum FsCommand {
    /// List a directory
    Ls {
        path: Option<String>,
        /// Read as of a commit, savepoint, or moment (e.g. @2h, @2026-08-01T12:00:00Z)
        #[arg(long)]
        at: Option<String>,
    },
    /// Print a file's bytes
    Cat {
        path: String,
        /// Read as of a commit, savepoint, or moment (e.g. @2h, @2026-08-01T12:00:00Z)
        #[arg(long)]
        at: Option<String>,
    },
    /// Write stdin (or --data) to a file as one commit
    Write {
        path: String,
        #[arg(long)]
        data: Option<String>,
    },
    /// Remove a file
    Rm { path: String },
    /// Move a file, symlink, or directory
    Mv { from: String, to: String },
    /// Copy a file or symlink (content is shared, not duplicated)
    Cp { from: String, to: String },
    /// Create a symlink
    Ln {
        /// What the link points at
        target: String,
        /// Where the link lives
        path: String,
    },
    /// Change mode bits, e.g. 755
    Chmod {
        /// Octal mode
        mode: String,
        path: String,
    },
}

#[derive(Subcommand)]
enum KvCommand {
    Get {
        key: String,
    },
    Set {
        key: String,
        value: String,
    },
    Del {
        key: String,
    },
    Keys {
        #[arg(default_value = "")]
        prefix: String,
    },
}

#[derive(Subcommand)]
enum ToolsCommand {
    /// List recent tool calls
    Recent {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Per-tool counts, outcomes, and durations
    Stats,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let base = match &cli.base {
        Some(base) => base.clone(),
        None => std::env::current_dir()?,
    };
    // The flag wins over the environment when both are set, but the environment is the
    // documented way: an argument is visible to every user on the machine through `ps` and
    // lands in shell history. AgentFS ships the argv path as its primary interface; this
    // reverses that emphasis.
    let key = cli
        .key
        .clone()
        .or_else(|| std::env::var("SURREALFS_KEY").ok());
    // `migrate` has to run before the store is opened, because opening is what applies
    // migrations — inspecting them afterwards would only ever report success.
    if let Command::Migrate { apply } = &cli.command {
        return migrate(&base, &cli.repo, *apply).await;
    }
    // Completions describe the command tree; opening a repository to print them would create
    // one as a side effect of asking for help.
    if let Command::Completions { shell } = &cli.command {
        use clap::CommandFactory;
        clap_complete::generate(
            *shell,
            &mut Cli::command(),
            "surrealfs",
            &mut std::io::stdout(),
        );
        return Ok(());
    }

    let options = SfsOptions::with_id_in(&base, &cli.repo);
    let options = match key.as_deref() {
        Some(hex) => options.with_key(hex)?,
        None => options,
    };
    let sfs = Surrealfs::open(options).await?;

    match cli.command {
        Command::Init { dir } => {
            if let Some(dir) = dir {
                let report = host::ingest(sfs.kernel(), &dir, &IngestOptions::default()).await?;
                println!(
                    "ingested {} files ({} bytes) from {}",
                    report.files,
                    report.bytes,
                    dir.display()
                );
                for (path, reason) in &report.skipped {
                    println!("  skipped {} ({reason})", path.display());
                }
            }
            let (head, root) = sfs.head().await?;
            println!(
                "repository '{}' ready at {}/.surrealfs/{}",
                cli.repo,
                base.display(),
                cli.repo
            );
            println!("head {head}");
            println!("root {root}");
        }
        Command::Diff { from, to } => {
            let kernel = sfs.kernel();
            let from = match from {
                Some(id) => CommitId::parse(&id)?,
                None => kernel.first_commit().await?,
            };
            let to = match to {
                Some(id) => CommitId::parse(&id)?,
                None => kernel.head().await?.head,
            };
            let changes = kernel.diff_commits(&from, &to).await?;
            if changes.is_empty() {
                println!("no changes");
            }
            for change in &changes {
                match change {
                    Change::Added(path, entry) if entry.is_dir() => println!("A {path}/"),
                    Change::Added(path, _) => println!("A {path}"),
                    Change::Removed(path, entry) if entry.is_dir() => println!("D {path}/"),
                    Change::Removed(path, _) => println!("D {path}"),
                    Change::Modified { path, .. } => println!("M {path}"),
                }
            }
        }
        Command::Apply {
            dir,
            from,
            to,
            dry_run,
            backup,
        } => {
            let kernel = sfs.kernel();
            let from = CommitId::parse(&from)?;
            let to = match to {
                Some(id) => CommitId::parse(&id)?,
                None => kernel.head().await?.head,
            };
            let opts = ApplyOptions {
                backup_dir: backup,
                dry_run,
            };
            let report = host::apply(kernel, &dir, &from, &to, &opts).await?;
            let verb = if dry_run { "would write" } else { "wrote" };
            for path in &report.written {
                println!("{verb} {path}");
            }
            let verb = if dry_run { "would remove" } else { "removed" };
            for path in &report.removed {
                println!("{verb} {path}");
            }
            if !report.backed_up.is_empty() {
                println!("backed up {} file(s)", report.backed_up.len());
            }
        }
        Command::Explain { path, limit } => {
            let history = sfs.kernel().explain(&path, limit).await?;
            if history.is_empty() {
                println!("no recorded changes to {path}");
            }
            for step in &history {
                let tool = step.tool_name.as_deref().unwrap_or("(no tool call)");
                println!(
                    "{} {} by {tool} at {}",
                    step.kind, step.commit, step.committed_at
                );
                if let Some(message) = &step.message {
                    println!("    {message}");
                }
            }
        }
        Command::Savepoint(cmd) => {
            let kernel = sfs.kernel();
            match cmd {
                SavepointCommand::Create { name, at, message } => {
                    let at = match at {
                        Some(reference) => Some(resolve(kernel, &reference).await?),
                        None => None,
                    };
                    let commit = kernel.savepoint(&name, at.as_ref(), message).await?;
                    println!("savepoint '{name}' -> {commit}");
                }
                SavepointCommand::List => {
                    for s in kernel.savepoints().await? {
                        let note = s.message.unwrap_or_default();
                        println!("{:<24} {} {note}", s.name, s.commit);
                    }
                }
            }
        }
        Command::Branch(cmd) => {
            let kernel = sfs.kernel();
            match cmd {
                BranchCommand::Create { name, at, message } => {
                    let at = match at {
                        Some(reference) => resolve(kernel, &reference).await?,
                        None => kernel.head().await?.head,
                    };
                    let name = BranchName::parse(&name)?;
                    kernel.fork(&name, &at, message).await?;
                    println!("branch '{name}' created at {at}");
                }
                BranchCommand::List => {
                    for b in kernel.branches().await? {
                        let marker = if b.name == kernel.branch().as_str() {
                            "*"
                        } else {
                            " "
                        };
                        println!("{marker} {:<20} {}", b.name, b.head);
                    }
                }
            }
        }
        Command::Revert { target, message } => {
            let kernel = sfs.kernel();
            let target = resolve(kernel, &target).await?;
            let receipt = kernel.revert_to(&target, message).await?;
            println!("reverted to {target}");
            println!("new commit {}", receipt.commit);
            println!("root {}", receipt.state_root);
        }
        Command::Export { file } => {
            let out = std::io::BufWriter::new(std::fs::File::create(&file)?);
            let stats = sfs
                .kernel()
                .store()
                .export_archive(sfs.kernel().repo(), out)
                .await?;
            println!(
                "exported {} commits, {} chunks ({} bytes), {} tree nodes to {}",
                stats.commits,
                stats.chunks,
                stats.bytes,
                stats.tree_nodes,
                file.display()
            );
        }
        Command::Import { file } => {
            let input = std::io::BufReader::new(std::fs::File::open(&file)?);
            // Verification happens before anything is written, so a bad archive cannot
            // leave a half-populated repository behind.
            let contents = surrealfs_store::read_archive(input)?;
            let stats = sfs
                .kernel()
                .store()
                .import_archive(sfs.kernel().repo(), contents)
                .await?;
            println!(
                "imported {} commits, {} chunks, {} branches, {} savepoints",
                stats.commits, stats.chunks, stats.branches, stats.snapshots
            );
        }
        Command::Gc { grace } => {
            let report = sfs.kernel().store().gc(sfs.kernel().repo(), grace).await?;
            println!(
                "reclaimed {} chunks ({} bytes) and {} tree nodes",
                report.chunks_removed, report.bytes_reclaimed, report.nodes_removed
            );
            if report.kept_within_grace > 0 {
                println!(
                    "kept {} unreferenced object(s) inside the {grace}s grace period",
                    report.kept_within_grace
                );
            }
        }
        Command::Run {
            dir,
            no_sandbox,
            allow_network,
            allow_read,
            message,
            command,
        } => {
            let (program, args) = command.split_first().expect("clap requires one argument");
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let report = surrealfs_run::run_in(
                sfs.kernel(),
                &dir,
                program,
                &args,
                &surrealfs_run::RunOptions {
                    confine: !no_sandbox,
                    allow_network,
                    extra_readable: allow_read,
                    message,
                    excludes: Vec::new(),
                },
            )
            .await?;

            match &report.commit {
                Some(commit) => println!("recorded {} file(s) as {}", report.files, commit.0),
                None => println!("nothing changed; no commit"),
            }
            if !report.confined {
                // Say it out loud. A reader of this commit later has no other way to know the
                // record might be incomplete.
                eprintln!(
                    "warning: ran without confinement; changes outside {} are not recorded",
                    dir.display()
                );
            }
            match report.exit_code {
                Some(0) => {}
                Some(code) => {
                    eprintln!("command exited {code}");
                    // Propagate the command's status: a script wrapping this needs to see it.
                    std::process::exit(code);
                }
                None => {
                    eprintln!("command was killed by a signal");
                    std::process::exit(1);
                }
            }
        }
        Command::Migrate { .. } | Command::Completions { .. } => {
            unreachable!("handled before the store is opened")
        }
        Command::Mcp => {
            // The MCP loop owns its handle for the life of the connection.
            let kernel = std::sync::Arc::new(
                surrealfs_kernel::Kernel::open(
                    sfs.kernel().store().clone(),
                    sfs.kernel().repo().clone(),
                )
                .await?,
            );
            surrealfs_mcp::serve_stdio(kernel).await?;
        }
        Command::Fs(cmd) => match cmd {
            FsCommand::Ls { path, at } => {
                let path = path.unwrap_or_else(|| "/".to_string());
                let entries = match &at {
                    // Reading a past moment goes through the kernel's historical view rather
                    // than the head-only SDK call.
                    Some(reference) => {
                        let kernel = sfs.kernel();
                        let commit = resolve(kernel, reference).await?;
                        let (ns, _) = kernel.state_at(&commit).await?;
                        surrealfs_kernel::view::list_dir(
                            kernel.store(),
                            kernel.repo(),
                            &ns,
                            &surrealfs_types::RepoPath::parse(&path)?,
                        )
                        .await?
                    }
                    None => sfs.fs().readdir(&path).await?,
                };
                for entry in entries {
                    let kind = if entry.is_dir { "dir" } else { "file" };
                    println!("{kind} {:>10}  {}", entry.size, entry.name);
                }
            }
            FsCommand::Cat { path, at } => {
                use std::io::Write;
                let bytes = match &at {
                    Some(reference) => {
                        let kernel = sfs.kernel();
                        let commit = resolve(kernel, reference).await?;
                        let root = kernel
                            .store()
                            .root_of_commit(kernel.repo(), &commit)
                            .await?;
                        kernel
                            .read_file_at(&root, &surrealfs_types::RepoPath::parse(&path)?)
                            .await?
                    }
                    None => sfs.fs().read_file(&path).await?,
                };
                std::io::stdout().write_all(&bytes)?;
            }
            FsCommand::Write { path, data } => {
                let bytes = match data {
                    Some(data) => data.into_bytes(),
                    None => {
                        use std::io::Read;
                        let mut buf = Vec::new();
                        std::io::stdin().read_to_end(&mut buf)?;
                        buf
                    }
                };
                let receipt = sfs.fs().write_file(&path, &bytes).await?;
                println!(
                    "committed {} (sequence {})",
                    receipt.commit, receipt.domain_sequence
                );
            }
            FsCommand::Rm { path } => {
                let receipt = sfs.fs().remove_file(&path).await?;
                println!(
                    "committed {} (sequence {})",
                    receipt.commit, receipt.domain_sequence
                );
            }
            FsCommand::Mv { from, to } => {
                let receipt = sfs.fs().rename(&from, &to).await?;
                println!("{from} -> {to} in {}", receipt.commit);
            }
            FsCommand::Cp { from, to } => {
                let receipt = sfs.fs().copy(&from, &to).await?;
                println!("{from} -> {to} in {}", receipt.commit);
            }
            FsCommand::Ln { target, path } => {
                let receipt = sfs.fs().symlink(&path, &target).await?;
                println!("{path} -> {target} in {}", receipt.commit);
            }
            FsCommand::Chmod { mode, path } => {
                let parsed = u32::from_str_radix(&mode, 8)
                    .map_err(|_| anyhow::anyhow!("mode must be octal, e.g. 755"))?;
                let receipt = sfs.fs().set_meta(&path, Some(parsed), None, None).await?;
                println!("{path} mode {mode} in {}", receipt.commit);
            }
        },
        Command::Kv(cmd) => match cmd {
            KvCommand::Get { key } => match sfs.kv().get(&key).await? {
                Some(value) => println!("{}", String::from_utf8_lossy(&value)),
                None => bail!("key not found: {key}"),
            },
            KvCommand::Set { key, value } => {
                let receipt = sfs.kv().set(&key, value.as_bytes()).await?;
                println!(
                    "committed {} (sequence {})",
                    receipt.commit, receipt.domain_sequence
                );
            }
            KvCommand::Del { key } => {
                let receipt = sfs.kv().delete(&key).await?;
                println!(
                    "committed {} (sequence {})",
                    receipt.commit, receipt.domain_sequence
                );
            }
            KvCommand::Keys { prefix } => {
                for key in sfs.kv().keys(&prefix).await? {
                    println!("{key}");
                }
            }
        },
        Command::Timeline { limit } => {
            for entry in sfs.timeline(limit).await? {
                println!(
                    "#{:<4} {} {:<24} {} mutation(s)  {}",
                    entry.domain_sequence,
                    entry.commit,
                    entry.message.as_deref().unwrap_or("-"),
                    entry.mutation_count,
                    entry.committed_at,
                );
            }
        }
        Command::Tools(ToolsCommand::Recent { limit }) => {
            for call in sfs.tools().recent(limit).await? {
                println!(
                    "{:<20} {:<10} {}",
                    call.tool_name, call.status, call.started_at
                );
            }
        }
        Command::Tools(ToolsCommand::Stats) => {
            let stats = sfs.kernel().tool_stats().await?;
            if stats.is_empty() {
                println!("no tool calls recorded");
            } else {
                println!(
                    "{:<20} {:>6} {:>6} {:>6} {:>8} {:>9}",
                    "TOOL", "CALLS", "OK", "FAIL", "RUNNING", "AVG_MS"
                );
            }
            for s in stats {
                let avg = s
                    .avg_duration_ms
                    .map(|v| format!("{v:.1}"))
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<20} {:>6} {:>6} {:>6} {:>8} {:>9}",
                    s.tool_name, s.calls, s.succeeded, s.failed, s.running, avg
                );
            }
        }
    }

    sfs.close().await?;
    Ok(())
}

/// Report migration state, and optionally apply what is pending.
///
/// Reporting opens without applying, so the answer describes the repository as it is rather than
/// as this command just made it.
async fn migrate(base: &std::path::Path, repo: &str, apply: bool) -> Result<()> {
    use surrealfs_store::{Store, StoreConfig, StoreEngine};

    let dir = base.join(".surrealfs").join(repo).join("db");
    if !dir.exists() {
        bail!("no repository at {}", dir.display());
    }

    let store = Store::open_with(
        StoreEngine::SurrealKv(dir.clone()),
        StoreConfig::read_only(),
    )
    .await?;
    let states = store.migration_status().await?;

    for state in &states {
        println!("{:<24} {}", state.id(), state.label());
    }

    let pending = states
        .iter()
        .filter(|s| matches!(s, surrealfs_store::MigrationState::Pending(_)))
        .count();
    let blocking: Vec<_> = states.iter().filter(|s| s.is_blocking()).collect();

    if !blocking.is_empty() {
        // These are not fixed by running the migration again, so saying "run --apply" would be
        // actively misleading.
        for state in &blocking {
            eprintln!("\n{}: {}", state.id(), state.label());
        }
        bail!("this repository needs manual attention before it can be opened for writes");
    }

    if pending == 0 {
        println!("\nup to date");
        return Ok(());
    }
    if !apply {
        println!("\n{pending} migration(s) would be applied; rerun with --apply");
        return Ok(());
    }

    // Dropping the read-only handle first: the engine allows one owner, and applying is just
    // opening normally.
    drop(store);
    Store::open_with(StoreEngine::SurrealKv(dir), StoreConfig::default()).await?;
    println!("\napplied {pending} migration(s)");
    Ok(())
}

/// Accept a commit id, a savepoint name, or a moment in time wherever a commit is expected.
///
/// Time references carry an `@` sigil: `@2h`, `@30m`, `@2026-08-01T12:00:00Z`. A bare timestamp
/// is deliberately not accepted, because savepoint names are free-form — without the sigil, what
/// a reference *meant* would depend on whether a savepoint happened to share its spelling, and
/// that ambiguity would resolve silently rather than as an error.
///
/// Every command that takes a commit goes through here, so all of them understand all three
/// forms without any of them knowing about time.
async fn resolve(kernel: &surrealfs_kernel::Kernel, reference: &str) -> Result<CommitId> {
    if let Some(time_ref) = reference.strip_prefix('@') {
        let at = resolve_time(time_ref)?;
        return Ok(kernel.commit_at_or_before(at).await?);
    }
    if let Ok(commit) = CommitId::parse(reference) {
        return Ok(commit);
    }
    Ok(kernel.resolve_savepoint(reference).await?)
}

/// Turn the text after an `@` into an instant: either an absolute timestamp or an offset back
/// from now.
fn resolve_time(text: &str) -> Result<std::time::SystemTime> {
    if let Some(at) = surrealfs_types::time::parse_rfc3339(text) {
        return Ok(at);
    }
    if let Some(ago) = surrealfs_types::time::parse_relative(text) {
        return std::time::SystemTime::now()
            .checked_sub(ago)
            .ok_or_else(|| anyhow::anyhow!("@{text} is further back than time goes"));
    }
    Err(anyhow::anyhow!(
        "cannot read @{text} as a time; use @2026-08-01T12:00:00Z or a duration like @2h, \
         @30m, @7d"
    ))
}
