//! A real FUSE mount, driven by real POSIX syscalls.
//!
//! Everything below `surrealfs-mount` is tested on any platform. What can only be tested here is
//! that the translation is right: that the kernel accepts our replies, that `ls` and `cat` and
//! `mv` see what they should, and that the inode discipline survives contact with a client that
//! caches aggressively and was written to assume a normal filesystem.
//!
//! Requires `/dev/fuse`, `CAP_SYS_ADMIN`, and an unconfined AppArmor profile. See
//! `docker/linux-test.Dockerfile` for the minimal invocation — notably, full `--privileged` is
//! not needed.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fuser::{Config, MountOption, Session, SessionACL};
use surrealfs_fuse::SurrealFuse;
use surrealfs_kernel::Kernel;
use surrealfs_mount::MountKernel;
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::RepositoryId;

/// A mounted filesystem plus everything that has to outlive it.
struct Mounted {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    mount: Arc<MountKernel>,
    kernel: Arc<Kernel>,
    runtime: tokio::runtime::Runtime,
    session: Option<fuser::BackgroundSession>,
}

impl Mounted {
    fn new(name: &str) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (kernel, mount) = runtime.block_on(async {
            let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
            let kernel = Arc::new(
                Kernel::open(store, RepositoryId::parse(name).unwrap())
                    .await
                    .unwrap(),
            );
            let mount = Arc::new(MountKernel::new(kernel.clone()).await.unwrap());
            (kernel, mount)
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mnt");
        fs::create_dir(&path).unwrap();

        let fs_impl = SurrealFuse::new(mount.clone(), runtime.handle().clone());
        // `Config` is `#[non_exhaustive]`, so it is built from its default rather than by
        // struct literal — which is the point of the attribute: a new field appearing upstream
        // must not silently change what this mount asks for.
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::FSName("surrealfs".into()),
            MountOption::NoAtime,
        ];
        config.acl = SessionACL::Owner;
        let session = Session::new(fs_impl, &path, &config)
            .expect("mounting requires /dev/fuse and CAP_SYS_ADMIN")
            .spawn()
            .unwrap();

        // The mount is asynchronous; give the kernel a moment to attach before touching it.
        std::thread::sleep(Duration::from_millis(200));

        Self {
            _dir: dir,
            path,
            mount,
            kernel,
            runtime,
            session: Some(session),
        }
    }

    fn at(&self, rel: &str) -> std::path::PathBuf {
        self.path.join(rel)
    }

    /// Unmount, then wait for the session thread.
    ///
    /// `join` alone deadlocks: it waits for the session to end, and a session only ends once the
    /// filesystem is unmounted, which nothing else here does.
    fn unmount(&mut self) {
        if let Some(session) = self.session.take() {
            session.umount_and_join().ok();
        }
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        self.unmount();
    }
}

#[test]
fn posix_operations_work_through_a_real_mount() {
    let m = Mounted::new("fuse-basic");

    // Create a directory tree the way any tool would.
    fs::create_dir(m.at("src")).unwrap();
    fs::write(m.at("src/main.rs"), b"fn main() {}").unwrap();
    assert_eq!(fs::read(m.at("src/main.rs")).unwrap(), b"fn main() {}");

    // Directory listing, including that `.` and `..` do not leak into readdir results.
    let mut names: Vec<String> = fs::read_dir(m.at("src"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["main.rs"]);

    // Metadata.
    let meta = fs::metadata(m.at("src/main.rs")).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.len(), 12);
    assert_ne!(meta.ino(), 0, "a real filesystem never reports inode 0");

    // Overwrite with longer content, then with SHORTER content. The shorter case is the one
    // that catches a dropped O_TRUNC: a longer replacement covers the old bytes and hides it.
    fs::write(m.at("src/main.rs"), b"fn main() { }").unwrap();
    assert_eq!(fs::read(m.at("src/main.rs")).unwrap(), b"fn main() { }");
    fs::write(m.at("src/main.rs"), b"fn m(){}").unwrap();
    assert_eq!(
        fs::read(m.at("src/main.rs")).unwrap(),
        b"fn m(){}",
        "a shorter overwrite left the old tail behind, so O_TRUNC was dropped"
    );
    assert_eq!(fs::metadata(m.at("src/main.rs")).unwrap().len(), 8);

    // Rename, remove, and directory removal.
    fs::rename(m.at("src/main.rs"), m.at("src/lib.rs")).unwrap();
    assert!(m.at("src/lib.rs").exists());
    assert!(!m.at("src/main.rs").exists());
    fs::remove_file(m.at("src/lib.rs")).unwrap();
    fs::remove_dir(m.at("src")).unwrap();
    assert!(!m.at("src").exists());
}

/// The inode discipline, checked by a client that caches. `fuser` is a low-level binding, so
/// nothing between us and the kernel would substitute a number if we got it wrong.
#[test]
fn inode_numbers_are_stable_and_survive_a_rename() {
    let m = Mounted::new("fuse-inodes");

    fs::create_dir(m.at("d")).unwrap();
    fs::write(m.at("d/f.txt"), b"body").unwrap();

    let first = fs::metadata(m.at("d/f.txt")).unwrap().ino();
    let second = fs::metadata(m.at("d/f.txt")).unwrap().ino();
    assert_eq!(first, second, "two stats of one file disagreed");
    assert_ne!(first, 0);

    // Distinct files get distinct numbers — the property `dofs` loses for pending creates.
    fs::write(m.at("d/g.txt"), b"other").unwrap();
    assert_ne!(first, fs::metadata(m.at("d/g.txt")).unwrap().ino());

    // A rename keeps the number, as it would on any real filesystem.
    fs::rename(m.at("d/f.txt"), m.at("d/renamed.txt")).unwrap();
    assert_eq!(
        fs::metadata(m.at("d/renamed.txt")).unwrap().ino(),
        first,
        "rename changed the inode number"
    );
}

#[test]
fn symlinks_and_hard_links_behave() {
    let m = Mounted::new("fuse-links");
    fs::write(m.at("real.txt"), b"content").unwrap();

    std::os::unix::fs::symlink("/real.txt", m.at("link.txt")).unwrap();
    let target = fs::read_link(m.at("link.txt")).unwrap();
    assert_eq!(target, Path::new("/real.txt"));
    assert!(
        fs::symlink_metadata(m.at("link.txt")).unwrap().is_symlink(),
        "lstat must see a symlink"
    );

    fs::hard_link(m.at("real.txt"), m.at("alias.txt")).unwrap();
    assert_eq!(fs::metadata(m.at("alias.txt")).unwrap().nlink(), 2);
    assert_eq!(fs::read(m.at("alias.txt")).unwrap(), b"content");
}

/// The decision a mount exists to protect: writing through it stages, and publishes nothing.
#[test]
fn writing_through_the_mount_does_not_publish_a_commit() {
    let m = Mounted::new("fuse-staging");
    let before = m.runtime.block_on(m.kernel.timeline(50)).unwrap().len();

    fs::write(m.at("draft.txt"), b"work in progress").unwrap();
    // Visible through the mount immediately...
    assert_eq!(fs::read(m.at("draft.txt")).unwrap(), b"work in progress");
    // ...and to nobody else.
    assert_eq!(
        m.runtime.block_on(m.kernel.timeline(50)).unwrap().len(),
        before,
        "a write through the mount invented a commit"
    );

    // Only an explicit publication moves the repository.
    m.runtime
        .block_on(m.mount.publish(Some("turn complete".into())))
        .unwrap();
    assert_eq!(
        m.runtime.block_on(m.kernel.timeline(50)).unwrap().len(),
        before + 1
    );
}

/// Errors have to arrive as the errno the caller expects, or ordinary tools misbehave.
#[test]
fn errors_arrive_as_the_right_errno() {
    let m = Mounted::new("fuse-errno");
    fs::create_dir(m.at("dir")).unwrap();
    fs::write(m.at("dir/child.txt"), b"x").unwrap();
    fs::write(m.at("file.txt"), b"y").unwrap();

    let missing = fs::read(m.at("nope.txt")).unwrap_err();
    assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));

    let not_empty = fs::remove_dir(m.at("dir")).unwrap_err();
    assert_eq!(not_empty.raw_os_error(), Some(libc::ENOTEMPTY));

    let is_a_dir = fs::remove_file(m.at("dir")).unwrap_err();
    assert!(matches!(
        is_a_dir.raw_os_error(),
        Some(libc::EISDIR) | Some(libc::EPERM)
    ));

    let not_a_dir = fs::read_dir(m.at("file.txt")).unwrap_err();
    assert_eq!(not_a_dir.raw_os_error(), Some(libc::ENOTDIR));
}

/// Larger than one chunk, so the extent path is exercised rather than the single-chunk shortcut.
#[test]
fn multi_chunk_files_round_trip_byte_for_byte() {
    let m = Mounted::new("fuse-chunks");

    // 700 KiB against a 256 KiB chunk: three chunks with a partial tail.
    let body: Vec<u8> = (0..700 * 1024).map(|i| (i % 251) as u8).collect();
    fs::write(m.at("big.bin"), &body).unwrap();

    assert_eq!(
        fs::metadata(m.at("big.bin")).unwrap().len(),
        body.len() as u64
    );
    assert_eq!(fs::read(m.at("big.bin")).unwrap(), body, "content differed");

    // A partial read from the middle, which is where an off-by-one in extent maths shows up.
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(m.at("big.bin")).unwrap();
    f.seek(SeekFrom::Start(300 * 1024)).unwrap();
    let mut buf = vec![0u8; 4096];
    f.read_exact(&mut buf).unwrap();
    assert_eq!(buf, &body[300 * 1024..300 * 1024 + 4096]);
}
