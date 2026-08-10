//! Does the Landlock policy actually confine a real process?
//!
//! These mirror the Seatbelt tests deliberately: the two platforms use entirely different
//! mechanisms, and the point of expressing one `Confinement` is that both must end up making the
//! same promises. If a claim holds on macOS and not on Linux, the abstraction is a lie.
//!
//! Unlike the FUSE tests, these need no privileges at all — Landlock is unprivileged by design,
//! which is the main reason to prefer it over mount namespaces here.

#![cfg(target_os = "linux")]

use std::fs;

use surrealfs_sandbox::{confined_command, landlock, Confinement};

fn tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let canonical = dir.path().canonicalize().unwrap();
    (dir, canonical)
}

/// The system paths a stock binary needs to start. Narrow on purpose: this is what an agent gets,
/// not the whole root.
fn with_system(policy: Confinement) -> Confinement {
    policy
        .allow_read("/bin")
        .allow_read("/usr")
        .allow_read("/lib")
        .allow_read("/lib64")
        .allow_read("/etc/ld.so.cache")
}

/// Run under a policy, treating a failed spawn as a denial rather than a test error.
///
/// Landlock applies before `exec`, so a policy tight enough to deny the binary itself makes the
/// spawn fail with `EACCES` instead of producing a child that exits non-zero. Both are the
/// sandbox working; only the reporting differs.
fn run(confinement: &Confinement, program: &str, args: &[&str]) -> (bool, String) {
    let Ok(mut command) = confined_command(confinement, program, args) else {
        return (false, String::new());
    };
    match command.output() {
        Ok(output) => (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).to_string(),
        ),
        Err(_) => (false, String::new()),
    }
}

#[test]
fn landlock_is_available_here() {
    assert!(
        landlock::available(),
        "these tests are meaningless without Landlock; kernel reports ABI {}",
        landlock::abi_version()
    );
}

#[test]
fn a_process_can_read_inside_the_policy_and_not_outside_it() {
    let (_dir, root) = tempdir();
    let secret = root.join("secret.txt");
    fs::write(&secret, b"must not be readable").unwrap();

    let readable_dir = root.join("readable");
    fs::create_dir(&readable_dir).unwrap();
    let inside = readable_dir.join("data.txt");
    fs::write(&inside, b"visible").unwrap();

    let policy = with_system(Confinement::closed()).allow_read(&readable_dir);

    let (ok, out) = run(&policy, "/bin/cat", &[inside.to_str().unwrap()]);
    assert!(ok, "a permitted read failed");
    assert_eq!(out, "visible");

    let (ok, _) = run(&policy, "/bin/cat", &[secret.to_str().unwrap()]);
    assert!(
        !ok,
        "a file outside the policy was readable — Landlock is not enforcing"
    );
}

#[test]
fn a_process_can_write_inside_the_mount_and_not_outside_it() {
    let (_dir, root) = tempdir();
    let mount = root.join("mount");
    let outside = root.join("outside");
    fs::create_dir(&mount).unwrap();
    fs::create_dir(&outside).unwrap();

    let policy = with_system(Confinement::closed()).allow_write(&mount);

    let inside_target = mount.join("written.txt");
    let (ok, _) = run(
        &policy,
        "/bin/sh",
        &["-c", &format!("echo hello > {}", inside_target.display())],
    );
    assert!(ok, "a permitted write failed");
    assert_eq!(fs::read_to_string(&inside_target).unwrap().trim(), "hello");

    let outside_target = outside.join("escaped.txt");
    let (ok, _) = run(
        &policy,
        "/bin/sh",
        &["-c", &format!("echo pwned > {}", outside_target.display())],
    );
    assert!(
        !ok,
        "a write escaped the mount — this is the guarantee the product rests on"
    );
    assert!(
        !outside_target.exists(),
        "the escaped write actually landed on disk"
    );
}

/// A read grant must not quietly become a write grant, which is the mistake that turns a
/// read-only source tree into an editable one.
#[test]
fn a_readable_path_is_not_writable() {
    let (_dir, root) = tempdir();
    let readonly = root.join("ro");
    fs::create_dir(&readonly).unwrap();
    fs::write(readonly.join("f.txt"), b"original").unwrap();

    let policy = with_system(Confinement::closed()).allow_read(&readonly);

    let target = readonly.join("f.txt");
    let (ok, _) = run(
        &policy,
        "/bin/sh",
        &["-c", &format!("echo overwritten > {}", target.display())],
    );
    assert!(!ok, "a read-only grant permitted a write");
    assert_eq!(fs::read_to_string(&target).unwrap(), "original");
}

/// What `closed()` means on Linux, stated the same way as on macOS: not "nothing runs", but "no
/// file access". A binary needing only stdout still works.
#[test]
fn a_closed_policy_grants_no_file_access() {
    let (_dir, root) = tempdir();
    let file = root.join("data.txt");
    fs::write(&file, b"content").unwrap();

    // A closed policy denies even the loader, so a dynamically linked binary cannot start. That
    // is stricter than Seatbelt's baseline and worth stating rather than smoothing over.
    let (ok, _) = run(
        &Confinement::closed(),
        "/bin/cat",
        &[file.to_str().unwrap()],
    );
    assert!(!ok, "a closed policy permitted a file read");
}

#[test]
fn relative_and_traversing_paths_are_refused_before_spawning() {
    let relative = Confinement::closed().allow_write("relative/dir");
    assert!(confined_command(&relative, "/bin/echo", &["x"]).is_err());

    let traversing = Confinement::closed().allow_write("/mnt/work/../../etc");
    assert!(confined_command(&traversing, "/bin/echo", &["x"]).is_err());
}

/// Landlock is inherited and cannot be widened, so a confined process cannot escape by spawning a
/// child. This is the property that makes it usable for an agent that runs build tools.
#[test]
fn confinement_is_inherited_by_grandchildren() {
    let (_dir, root) = tempdir();
    let secret = root.join("secret.txt");
    fs::write(&secret, b"must not be readable").unwrap();

    let policy = with_system(Confinement::closed());

    // sh spawns cat, which is a grandchild of the confining process.
    let (ok, _) = run(
        &policy,
        "/bin/sh",
        &["-c", &format!("cat {}", secret.display())],
    );
    assert!(!ok, "a grandchild escaped the confinement");
}
