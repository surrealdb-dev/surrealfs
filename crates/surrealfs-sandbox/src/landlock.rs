//! Linux confinement via Landlock.
//!
//! Landlock is the right analogue of Seatbelt and, for this use, a better mechanism than mount
//! namespaces: it is **unprivileged**, path-based, and deny-by-default. Confining an agent
//! therefore needs no `CAP_SYS_ADMIN`, no `unshare`, and no root — which matters, because a
//! sandbox that requires privilege to enter tends to be skipped in the environments that most
//! need it.
//!
//! The model is self-restriction rather than supervision: a process narrows *its own* rights, and
//! the restriction is inherited by everything it later execs and can never be widened again. So
//! the rules are applied in the forked child between `fork` and `exec`, which is what
//! `pre_exec` exists for.
//!
//! Landlock covers the filesystem and, from ABI 4, TCP bind and connect. It does not cover
//! everything the Seatbelt profile does — there is no equivalent of denying arbitrary mach
//! services, because the concept does not exist here. Where the two platforms genuinely differ,
//! this module confines what Linux can confine rather than pretending to parity.

use std::path::PathBuf;

use landlock::{
    Access, AccessFs, AccessNet, BitFlags, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, RulesetStatus, ABI,
};
use surrealfs_types::SfsError;

use crate::Confinement;

/// The ABI to target.
///
/// V4 adds TCP restrictions, which is what lets `allow_network` mean something here. A kernel
/// older than the target degrades gracefully via `set_compatibility`, reporting partial
/// enforcement rather than failing — and the caller is told, because silently applying less
/// confinement than asked for is the failure mode this whole module exists to avoid.
const TARGET_ABI: ABI = ABI::V4;

/// What `restrict_self` achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    /// Every requested restriction is in force.
    Full,
    /// The kernel is older than the target ABI and enforced what it could.
    Partial,
    /// Landlock is unavailable — nothing was enforced.
    None,
}

/// Apply this policy to the current process, permanently.
///
/// Call between `fork` and `exec`. Returns what was actually enforced rather than a bare success,
/// because "Landlock is compiled out of this kernel" and "the agent is confined" must never be
/// the same return value to a caller that is about to run untrusted code.
pub fn restrict_current_process(confinement: &Confinement) -> Result<Enforcement, SfsError> {
    let fs_ro = AccessFs::from_read(TARGET_ABI);
    let fs_rw = AccessFs::from_all(TARGET_ABI);

    let mut ruleset = Ruleset::default()
        .set_compatibility(landlock::CompatLevel::BestEffort)
        .handle_access(fs_rw)
        .map_err(rule_err)?;

    if !confinement.network_allowed() {
        ruleset = ruleset
            .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)
            .map_err(rule_err)?;
    }

    let mut created = ruleset.create().map_err(rule_err)?;

    // Read-only grants first, then writable ones. A path appearing in both ends up with the union,
    // which is what `allow_write` promises.
    created = add_paths(created, confinement.readable(), fs_ro)?;
    created = add_paths(created, confinement.writable(), fs_rw)?;

    let status = created.restrict_self().map_err(rule_err)?;
    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => Enforcement::Full,
        RulesetStatus::PartiallyEnforced => Enforcement::Partial,
        RulesetStatus::NotEnforced => Enforcement::None,
    })
}

fn add_paths(
    ruleset: RulesetCreated,
    paths: &[PathBuf],
    access: BitFlags<AccessFs>,
) -> Result<RulesetCreated, SfsError> {
    let mut ruleset = ruleset;
    for path in paths {
        // A path that does not exist cannot be opened, and Landlock rules are anchored to an open
        // descriptor. Skipping is right rather than fatal: a mount point is routinely created
        // after the policy is built, and the rule grants nothing that the missing path would have.
        let Ok(fd) = PathFd::new(path) else {
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access))
            .map_err(rule_err)?;
    }
    Ok(ruleset)
}

fn rule_err<E: std::fmt::Display>(e: E) -> SfsError {
    SfsError::Storage(format!("landlock: {e}"))
}

/// The Landlock ABI this kernel implements, or 0 if it has none.
///
/// Asked of the kernel directly rather than inferred: `landlock_create_ruleset` with the version
/// flag and a null attribute is the documented probe, and it answers without creating anything.
/// The crate offers no equivalent, and guessing from `uname` would be wrong on any kernel built
/// without `CONFIG_SECURITY_LANDLOCK` or booted without it in the LSM list.
pub fn abi_version() -> i64 {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    // SAFETY: a null attribute pointer with size 0 is exactly what the version probe requires.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if rc > 0 {
        rc
    } else {
        0
    }
}

/// Whether this kernel can confine anything at all.
pub fn available() -> bool {
    abi_version() > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must answer rather than panic, on any kernel — including one with no Landlock.
    #[test]
    fn the_abi_probe_answers_without_panicking() {
        let abi = abi_version();
        assert!(abi >= 0, "the probe returned a negative ABI: {abi}");
        assert_eq!(available(), abi > 0);
    }

    /// This kernel is the one the tests below actually run against, so record what it supports.
    #[test]
    fn the_target_abi_is_reachable_here() {
        if !available() {
            eprintln!("skipping: this kernel has no Landlock");
            return;
        }
        assert!(
            abi_version() >= ABI::V1 as i64,
            "Landlock reported an ABI below V1"
        );
    }
}
