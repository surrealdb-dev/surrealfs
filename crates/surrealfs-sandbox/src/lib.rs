//! Confining an agent process to what it is allowed to touch.
//!
//! The point of running an agent against a SurrealFS mount is that every change it makes is
//! recorded. That guarantee is only as good as the agent's inability to write anywhere else, so
//! confinement is part of the product rather than a deployment detail: an agent that can edit
//! `~/.ssh` outside the mount has escaped the provenance graph, not merely the filesystem.
//!
//! The policy is expressed once, here, and rendered per platform — Seatbelt on macOS, Landlock on
//! Linux. Both start from **deny everything** and add back only what was asked for, because the
//! opposite order fails open: a policy built by subtracting from full access grants anything its
//! author forgot to think of.
//!
//! Landlock rather than mount namespaces on Linux, because it is unprivileged: confining an agent
//! needs no `CAP_SYS_ADMIN`, no `unshare`, and no root. A sandbox that requires privilege to enter
//! is one that gets skipped in the environments that most need it.

use std::path::{Path, PathBuf};

use surrealfs_types::SfsError;

#[cfg(target_os = "linux")]
pub mod landlock;
pub mod seatbelt;

pub use seatbelt::seatbelt_profile;

/// Build a command that runs `program` under this policy.
///
/// The returned `Command` has not been spawned, so the caller can still set environment, working
/// directory, and stdio. This wraps `sandbox-exec` with a generated Seatbelt profile.
#[cfg(target_os = "macos")]
pub fn confined_command(
    confinement: &Confinement,
    program: &str,
    args: &[&str],
) -> Result<std::process::Command, SfsError> {
    confinement.validate()?;
    let mut cmd = std::process::Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-p").arg(seatbelt_profile(&confinement.resolved()));
    cmd.arg(program);
    cmd.args(args);
    Ok(cmd)
}

/// Build a command that runs `program` confined by Landlock.
///
/// Landlock restricts the calling process and everything it execs, so the rules are applied in
/// the forked child just before `exec` — that is what `pre_exec` is for. Two checks stand between
/// a caller and an unconfined agent: this function refuses to build a command at all if the
/// kernel has no Landlock, and the child refuses to `exec` if applying the ruleset enforced
/// nothing. Neither is a silent path to running untrusted code with full rights.
#[cfg(target_os = "linux")]
pub fn confined_command(
    confinement: &Confinement,
    program: &str,
    args: &[&str],
) -> Result<std::process::Command, SfsError> {
    use std::os::unix::process::CommandExt;

    confinement.validate()?;
    if !landlock::available() {
        return Err(SfsError::Storage(
            "this kernel has no Landlock support; refusing to run unconfined".into(),
        ));
    }

    let policy = confinement.resolved();
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);

    // SAFETY: the closure runs in the forked child before exec. It calls only the Landlock
    // syscalls and allocates nothing that could deadlock against a lock held across the fork.
    unsafe {
        cmd.pre_exec(move || match landlock::restrict_current_process(&policy) {
            Ok(landlock::Enforcement::None) => Err(std::io::Error::other(
                "landlock enforced nothing; refusing to exec unconfined",
            )),
            Ok(_) => Ok(()),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        });
    }
    Ok(cmd)
}

/// What a confined process may do.
///
/// Construction starts closed. Every permission is an explicit addition, so a policy that says
/// nothing permits nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Confinement {
    readable: Vec<PathBuf>,
    writable: Vec<PathBuf>,
    network: bool,
}

impl Confinement {
    /// A policy that permits nothing at all.
    pub fn closed() -> Self {
        Self::default()
    }

    /// The usual shape: the agent may read the system it needs to run, and write only into its
    /// mount.
    ///
    /// System paths are read-only and deliberately narrow — an interpreter and its libraries, not
    /// the whole root. `/etc` is *not* included: it holds credentials and host configuration that
    /// an agent editing a repository has no reason to read. The set differs by platform because
    /// the platforms genuinely differ; naming macOS paths on Linux would grant nothing and hide
    /// the fact that nothing was granted.
    pub fn for_mount(mount: impl AsRef<Path>) -> Self {
        let base = Self::closed()
            .allow_read("/bin")
            .allow_read("/usr/bin")
            .allow_read("/usr/lib")
            .allow_read("/usr/share");
        #[cfg(target_os = "macos")]
        let base = base
            .allow_read("/System/Library")
            .allow_read("/Library/Developer");
        #[cfg(target_os = "linux")]
        let base = base.allow_read("/lib").allow_read("/lib64");
        base.allow_write(mount)
    }

    /// Permit reads under `path`.
    pub fn allow_read(mut self, path: impl AsRef<Path>) -> Self {
        self.readable.push(path.as_ref().to_path_buf());
        self
    }

    /// Permit reads and writes under `path`.
    ///
    /// Writable implies readable: a process that can write a file it cannot read is a policy bug
    /// dressed as a permission, and every caller would otherwise have to remember to say both.
    pub fn allow_write(mut self, path: impl AsRef<Path>) -> Self {
        self.writable.push(path.as_ref().to_path_buf());
        self
    }

    /// Permit outbound network access.
    ///
    /// Off by default. An agent that can reach the network can exfiltrate the repository it was
    /// given, which is a different risk from the one confinement is usually reached for, so it
    /// has to be asked for by name.
    pub fn allow_network(mut self) -> Self {
        self.network = true;
        self
    }

    pub fn readable(&self) -> &[PathBuf] {
        &self.readable
    }

    /// Writable paths. Each is readable too, so this is a subset of what reads are permitted.
    pub fn writable(&self) -> &[PathBuf] {
        &self.writable
    }

    pub fn network_allowed(&self) -> bool {
        self.network
    }

    /// Resolve every path to its canonical form, following symlinks.
    ///
    /// This is required for correctness, not tidiness: Seatbelt matches on the path the kernel
    /// resolves to, so a policy naming `/tmp/x` grants nothing to a process touching
    /// `/private/tmp/x`, and on macOS those are the same directory. A policy written the obvious
    /// way silently confines nothing it was meant to permit — it fails closed, so the symptom is
    /// an agent that mysteriously cannot write to its own mount.
    ///
    /// Paths that do not exist yet are kept as written. A mount point is often created after the
    /// policy is built, and refusing to describe it would be worse than describing it literally.
    pub fn resolved(&self) -> Self {
        let resolve = |paths: &Vec<PathBuf>| -> Vec<PathBuf> {
            paths
                .iter()
                .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
                .collect()
        };
        Confinement {
            readable: resolve(&self.readable),
            writable: resolve(&self.writable),
            network: self.network,
        }
    }

    /// Every path this policy mentions, absolute and lexically normalised.
    ///
    /// Relative paths are rejected rather than resolved against the current directory: a policy
    /// whose meaning depends on where the daemon happened to be started is not a policy.
    pub fn validate(&self) -> Result<(), SfsError> {
        for path in self.readable.iter().chain(self.writable.iter()) {
            if !path.is_absolute() {
                return Err(SfsError::InvalidPath(format!(
                    "confinement paths must be absolute, got {}",
                    path.display()
                )));
            }
            if path.components().any(|c| c.as_os_str() == "..") {
                return Err(SfsError::InvalidPath(format!(
                    "confinement paths must not contain `..`, got {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }
}
