//! Does the profile actually confine a real process?
//!
//! The unit tests prove the generated text says the right thing. That is not the same claim, and
//! the difference is exactly the failure mode found in ContextFS, whose 64 MiB write-buffer cap
//! exists as a constant with zero call sites: it reads as implemented and enforces nothing. So
//! these tests spawn actual processes under actual profiles and check what the kernel does.
//!
//! macOS only. The Linux namespace path has to be verified where it can be run, and
//! `confined_command` returns an error there rather than an unconfined command, so a mistake
//! fails loudly instead of running an agent with no sandbox at all.

#![cfg(target_os = "macos")]

use std::fs;

use surrealfs_sandbox::{confined_command, Confinement};

/// A temp directory and its canonical path.
///
/// macOS hands out `/var/folders/...`, which resolves to `/private/var/folders/...`, and Seatbelt
/// matches what the kernel resolved. Tests must therefore use the canonical path on both sides —
/// in the policy and in the argument handed to the child — or they measure the symlink rather
/// than the sandbox.
fn tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let canonical = dir.path().canonicalize().unwrap();
    (dir, canonical)
}

/// Run a command under a policy and return (exit status success, stdout).
fn run(confinement: &Confinement, program: &str, args: &[&str]) -> (bool, String) {
    let output = confined_command(confinement, program, args)
        .expect("building the command")
        .output()
        .expect("spawning sandbox-exec");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
    )
}

#[test]
fn a_process_can_read_inside_the_policy_and_not_outside_it() {
    let (_dir, root) = tempdir();
    let secret = root.join("secret.txt");
    fs::write(&secret, b"must not be readable").unwrap();

    // A nested layout: readable/ is granted, and the secret sits beside it rather than inside.
    let readable_dir = root.join("readable");
    fs::create_dir(&readable_dir).unwrap();
    let inside = readable_dir.join("data.txt");
    fs::write(&inside, b"visible").unwrap();

    let policy = Confinement::closed()
        .allow_read("/bin")
        .allow_read("/usr/lib")
        .allow_read("/System")
        .allow_read(&readable_dir);

    let (ok, out) = run(&policy, "/bin/cat", &[inside.to_str().unwrap()]);
    assert!(ok, "a permitted read failed");
    assert_eq!(out, "visible");

    let (ok, _) = run(&policy, "/bin/cat", &[secret.to_str().unwrap()]);
    assert!(
        !ok,
        "a file outside the policy was readable — the sandbox is not enforcing"
    );
}

#[test]
fn a_process_can_write_inside_the_mount_and_not_outside_it() {
    let (_dir, root) = tempdir();
    let mount = root.join("mount");
    let outside = root.join("outside");
    fs::create_dir(&mount).unwrap();
    fs::create_dir(&outside).unwrap();

    let policy = Confinement::closed()
        .allow_read("/bin")
        .allow_read("/usr/lib")
        .allow_read("/System")
        .allow_write(&mount);

    // /bin/sh redirection is the simplest way to make a write syscall from a stock binary.
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

/// The profile is generated from paths an agent can influence, so the injection defence has to
/// hold against the real parser, not only against our own string assertions.
#[test]
fn a_crafted_path_cannot_widen_the_policy_in_the_real_parser() {
    let (_dir, root) = tempdir();
    let secret = root.join("secret.txt");
    fs::write(&secret, b"must not be readable").unwrap();

    let policy = Confinement::closed()
        .allow_read("/bin")
        .allow_read("/usr/lib")
        .allow_read("/System")
        // A path that tries to close the literal and append a blanket allow.
        .allow_read(r#"/tmp/x")) (allow file-read* (subpath "/"#);

    let (ok, _) = run(&policy, "/bin/cat", &[secret.to_str().unwrap()]);
    assert!(
        !ok,
        "a crafted path widened the policy once macOS parsed it — escaping is insufficient"
    );
}

/// What `closed()` actually means.
///
/// It is not "nothing runs" — the baseline rules that let any process start at all are
/// unconditional, so a binary needing nothing but stdout still works. It is "no file access
/// beyond that baseline", which is the property worth having and the one to assert. A test
/// claiming the stronger thing would pass only by accident of which binary it picked.
#[test]
fn a_closed_policy_grants_no_file_access() {
    let (_dir, root) = tempdir();
    let file = root.join("data.txt");
    fs::write(&file, b"content").unwrap();

    // Needs nothing but stdout, so the baseline suffices.
    let (ok, out) = run(&Confinement::closed(), "/bin/echo", &["hello"]);
    assert!(ok, "the baseline must let a process start");
    assert_eq!(out.trim(), "hello");

    // Reading any file is refused, because nothing was granted.
    let (ok, _) = run(
        &Confinement::closed(),
        "/bin/cat",
        &[file.to_str().unwrap()],
    );
    assert!(!ok, "a closed policy permitted a file read");
}

/// Relative paths are refused before anything is spawned, because a policy whose meaning depends
/// on the daemon's working directory is not a policy.
#[test]
fn relative_and_traversing_paths_are_refused_before_spawning() {
    let relative = Confinement::closed().allow_write("relative/dir");
    assert!(confined_command(&relative, "/bin/echo", &["x"]).is_err());

    let traversing = Confinement::closed().allow_write("/mnt/work/../../etc");
    assert!(confined_command(&traversing, "/bin/echo", &["x"]).is_err());
}
