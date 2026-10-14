// Test Fxfs in the guest's kernel (fxdax's TASK-26.1 and TASK-26.6).
//
// The host boots the kernel fxdax's scripts/build-guest-kernel.sh builds, which
// FXDAX_GUEST_KERNEL names, with an extent-store device on a new store. The
// guest mounts the store's filesystem with the kernel's Fxfs, writes, links,
// renames and removes, unmounts and mounts again. After the VM exits, the host
// mounts the store with its own Fxfs and finds what the guest wrote.
//
// fxfs-guest: the host makes an fxdax filesystem on the store first, with a
// file, and a directory holding a larger file, which the guest lists and reads.
// fxfs-guest-format: the store holds nothing, and the guest's Fxfs formats it.
// fxfs-guest-crash: the guest formats the store, writes a file it fsyncs and
// files it doesn't, and stops without unmounting. The host checks the store's
// filesystem (fsck), and finds the file the guest fsynced.

use macros::{guest, host};

/// How a test makes and leaves the filesystem.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    HostFormats,
    GuestFormats,
    Crash,
}

pub struct TestFxfsGuest {
    mode: Mode,
}

impl TestFxfsGuest {
    /// fxfs-guest.
    pub fn host_formats() -> Self {
        Self { mode: Mode::HostFormats }
    }

    /// fxfs-guest-format.
    pub fn guest_formats() -> Self {
        Self { mode: Mode::GuestFormats }
    }

    /// fxfs-guest-crash.
    pub fn crash() -> Self {
        Self { mode: Mode::Crash }
    }
}

/// What the crash test's guest fsyncs.
fn durable() -> Vec<u8> {
    pattern((1 << 20) + 4321, 0x3c)
}

/// The small file's contents.
#[cfg_attr(feature = "host", allow(dead_code))]
const HELLO: &[u8] = b"Hello from the host's Fxfs\n";

/// The larger file's size: many blocks, and not a whole number of them.
#[cfg_attr(feature = "host", allow(dead_code))]
const NESTED_SIZE: usize = (2 << 20) + 12345;

/// The larger file's contents: a pattern no block repeats.
#[cfg_attr(feature = "host", allow(dead_code))]
fn nested() -> Vec<u8> {
    pattern(NESTED_SIZE, 0)
}

/// `len` bytes of a pattern no block repeats, which `seed` varies.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 ^ (i / 4096) as u8 ^ seed).collect()
}

/// What the guest writes, which the host reads back: more than one of the kernel's
/// chunks of writes, and not a whole number of blocks, then an append.
fn written() -> Vec<u8> {
    let mut data = pattern((3 << 20) + 777, 0x5a);
    data.extend_from_slice(b"appended\n");
    data
}

#[host]
mod host {
    use super::*;

    use crate::common::setup_fs_and_enter;
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
    use futures::executor::block_on;
    use fxdax::{Filesystem, Inode, Owner, Storage};
    use krun_sys::*;
    use nix::libc;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::ptr::null;

    /// The format libkrun loads the kernel in: an Image on arm64, an ELF vmlinux on x86-64.
    const KERNEL_FORMAT: u32 = if cfg!(target_arch = "aarch64") {
        KRUN_KERNEL_FORMAT_RAW
    } else {
        KRUN_KERNEL_FORMAT_ELF
    };

    /// The store's capacity.
    const CAPACITY: u64 = 64 << 20;

    type KrunAddExtentStoreFn = unsafe extern "C" fn(
        ctx_id: u32,
        c_tag: *const std::ffi::c_char,
        c_path: *const std::ffi::c_char,
        capacity: u64,
        shm_size: u64,
    ) -> i32;

    fn get_krun_add_extent_store() -> KrunAddExtentStoreFn {
        let symbol = CString::new("krun_add_extent_store").unwrap();
        let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr()) };
        assert!(!ptr.is_null(), "krun_add_extent_store not found");
        unsafe { std::mem::transmute(ptr) }
    }

    impl Test for TestFxfsGuest {
        fn should_run(&self) -> ShouldRun {
            if unsafe { krun_call_u32!(krun_has_feature(KRUN_FEATURE_FXDAX.into())) }.ok()
                != Some(1)
            {
                return ShouldRun::No("libkrun compiled without FXDAX");
            }
            if std::env::var_os("FXDAX_GUEST_KERNEL").is_none() {
                return ShouldRun::No("FXDAX_GUEST_KERNEL does not name a guest kernel");
            }
            ShouldRun::Yes
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let path = test_setup.tmp_dir.join("extents");
            if self.mode == Mode::HostFormats {
                block_on(async {
                    let fs = Filesystem::format(Storage::create(&path, CAPACITY)?).await?;
                    let owner = Owner { uid: 0, gid: 0 };
                    let hello = fs.create(Inode::ROOT, b"hello", 0o644, owner).await?;
                    fs.write(hello.inode, 0, HELLO).await?;
                    let dir = fs.mkdir(Inode::ROOT, b"dir", 0o750, owner).await?;
                    let file = fs.create(dir.inode, b"nested", 0o600, owner).await?;
                    fs.write(file.inode, 0, &nested()).await?;
                    fs.close().await?;
                    anyhow::Ok(())
                })?;
            }

            let krun_add_extent_store = get_krun_add_extent_store();
            let kernel = CString::new(std::env::var("FXDAX_GUEST_KERNEL")?)?;
            let store = CString::new(path.as_os_str().as_bytes())?;
            unsafe {
                krun_call!(krun_set_log_level(KRUN_LOG_LEVEL_WARN))?;
                let ctx = krun_call_u32!(krun_create_ctx())?;
                krun_call!(krun_set_vm_config(ctx, 2, 512))?;
                // libkrun's own command line, as with libkrunfw's kernel.
                krun_call!(krun_set_kernel(
                    ctx,
                    kernel.as_ptr(),
                    KERNEL_FORMAT,
                    null(),
                    null()
                ))?;
                krun_call!(krun_add_extent_store(
                    ctx,
                    c"extents".as_ptr(),
                    store.as_ptr(),
                    CAPACITY,
                    0,
                ))?;
                setup_fs_and_enter(ctx, test_setup)?;
            }
            unreachable!()
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            if output != "OK\n" {
                return TestOutcome::Fail(format!("expected exactly \"OK\\n\", got {output:?}"));
            }
            let path = test_setup.tmp_dir.join("extents");
            if self.mode == Mode::Crash {
                // The filesystem the guest left without unmounting is consistent, and holds
                // what the guest fsynced.
                let storage = block_on(Storage::open(&path).unwrap().check()).unwrap();
                let fs = block_on(Filesystem::mount(storage)).unwrap();
                let file = block_on(fs.lookup(Inode::ROOT, b"durable")).unwrap();
                assert_eq!(file.size, durable().len() as u64);
                let mut contents = vec![0; durable().len()];
                let mut read = 0;
                while read < contents.len() {
                    let n =
                        block_on(fs.read(file.inode, read as u64, &mut contents[read..])).unwrap();
                    assert_ne!(n, 0, "the file ends early, at {read}");
                    read += n;
                }
                assert_eq!(contents, durable());
                block_on(fs.close()).unwrap();
                return TestOutcome::Pass;
            }
            // What the guest wrote is in the store, for the host's Fxfs as for the guest's.
            let fs = block_on(Filesystem::mount(Storage::open(&path).unwrap())).unwrap();
            let lookup = |parent, name: &str| block_on(fs.lookup(parent, name.as_bytes())).unwrap();
            let dir = lookup(Inode::ROOT, "guest-dir");
            let file = lookup(dir.inode, "renamed");
            assert_eq!(file.size, written().len() as u64);
            assert_eq!(file.perm, 0o640);
            assert_eq!(file.nlink, 2);
            let mut contents = vec![0; written().len()];
            let mut read = 0;
            while read < contents.len() {
                let n = block_on(fs.read(file.inode, read as u64, &mut contents[read..])).unwrap();
                assert_ne!(n, 0, "the file ends early, at {read}");
                read += n;
            }
            assert_eq!(contents, written());
            assert_eq!(lookup(Inode::ROOT, "link").inode, file.inode);
            let symlink = lookup(Inode::ROOT, "symlink");
            assert_eq!(block_on(fs.readlink(symlink.inode)).unwrap(), b"guest-dir/renamed");
            // Removed by the guest, or never made.
            assert!(block_on(fs.lookup(Inode::ROOT, b"hello")).is_err());
            block_on(fs.close()).unwrap();
            TestOutcome::Pass
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    use nix::libc;
    use std::ffi::CString;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// What the guest writes through a mapping: three pages and a bit.
    fn mapped() -> Vec<u8> {
        pattern(3 * 4096 + 100, 0xa5)
    }

    const MNT: &str = "/mnt/fxfs";

    fn mount(source: &str) -> std::io::Result<()> {
        mount_at(source, MNT)
    }

    fn mount_at(source: &str, target: &str) -> std::io::Result<()> {
        let source = CString::new(source).unwrap();
        let target = CString::new(target).unwrap();
        let fstype = CString::new("fxfs").unwrap();
        let ret = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        if ret == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn unmount() {
        unmount_at(MNT)
    }

    fn unmount_at(target: &str) {
        let target = CString::new(target).unwrap();
        let ret = unsafe { libc::umount(target.as_ptr()) };
        assert_eq!(ret, 0, "umount: {}", std::io::Error::last_os_error());
    }

    /// Makes `dir`, and in it what `check_written` finds.
    fn write(dir: &str) {
        fs::create_dir(dir).unwrap();
        assert!(fs::metadata(dir).unwrap().is_dir());
        assert_eq!(
            fs::create_dir(dir).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );

        let path = format!("{dir}/file");
        let data = written();
        let (body, appended) = data.split_at(data.len() - b"appended\n".len());
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(body).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(appended).unwrap();
        drop(file);
        assert_eq!(fs::read(&path).unwrap(), data);

        // Truncation: shrinking, then growing with zeros.
        let scratch = format!("{dir}/scratch");
        fs::write(&scratch, b"0123456789").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&scratch).unwrap();
        file.set_len(4).unwrap();
        assert_eq!(fs::read(&scratch).unwrap(), b"0123");
        file.set_len(6).unwrap();
        assert_eq!(fs::read(&scratch).unwrap(), b"0123\0\0");
        drop(file);
        fs::remove_file(&scratch).unwrap();
        assert_eq!(
            fs::metadata(&scratch).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        fs::hard_link(&path, format!("{MNT}/link")).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().nlink(), 2);
        fs::rename(&path, format!("{dir}/renamed")).unwrap();
        std::os::unix::fs::symlink("guest-dir/renamed", format!("{MNT}/symlink")).unwrap();

        // A directory, removed once empty.
        let gone = format!("{dir}/gone");
        fs::create_dir(&gone).unwrap();
        fs::write(format!("{gone}/f"), b"x").unwrap();
        assert_eq!(
            fs::remove_dir(&gone).unwrap_err().raw_os_error(),
            Some(libc::ENOTEMPTY)
        );
        fs::remove_file(format!("{gone}/f")).unwrap();
        fs::remove_dir(&gone).unwrap();

        // Written through a shared mapping, which writeback writes.
        let mapped_path = format!("{dir}/mapped");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&mapped_path)
            .unwrap();
        let data = mapped();
        file.set_len(data.len() as u64).unwrap();
        unsafe {
            let addr = libc::mmap(
                std::ptr::null_mut(),
                data.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(addr, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
            std::ptr::copy_nonoverlapping(data.as_ptr(), addr.cast(), data.len());
            assert_eq!(libc::msync(addr, data.len(), libc::MS_SYNC), 0);
            assert_eq!(libc::munmap(addr, data.len()), 0);
        }
        drop(file);
        assert_eq!(fs::read(&mapped_path).unwrap(), data);

        // A program run from the filesystem: this one, which fails without a test's name.
        let program = format!("{dir}/program");
        fs::copy(std::env::current_exe().unwrap(), &program).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let output = std::process::Command::new(&program).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Missing test name argument"),
            "{output:?}"
        );
        fs::remove_file(&program).unwrap();

        let statfs = nix::sys::statfs::statfs(MNT).unwrap();
        assert!(statfs.blocks_free() < statfs.blocks());
    }

    /// Reads what the host made, and removes its file while it is open, which keeps it readable
    /// through its descriptor.
    fn read_hosts() {
        assert_eq!(names(MNT), ["dir", "hello"]);
        assert_eq!(fs::read(format!("{MNT}/hello")).unwrap(), HELLO);
        let hello = fs::metadata(format!("{MNT}/hello")).unwrap();
        assert!(hello.is_file());
        assert_eq!(hello.permissions().mode(), 0o100644);
        assert_eq!(hello.len(), HELLO.len() as u64);

        let dir = fs::metadata(format!("{MNT}/dir")).unwrap();
        assert!(dir.is_dir());
        assert_eq!(dir.permissions().mode(), 0o040750);
        assert_eq!(names(&format!("{MNT}/dir")), ["nested"]);
        assert_eq!(fs::read(format!("{MNT}/dir/nested")).unwrap(), nested());

        let hello = fs::File::open(format!("{MNT}/hello")).unwrap();
        fs::remove_file(format!("{MNT}/hello")).unwrap();
        let mut contents = Vec::new();
        (&hello).read_to_end(&mut contents).unwrap();
        assert_eq!(contents, HELLO);
        drop(hello);
        assert_eq!(names(MNT), ["dir"]);
    }

    /// What `write` made, after mounting again, beside what the host made, if it did.
    fn check_written(host_formats: bool) {
        let mut expected = vec!["guest-dir", "link", "symlink"];
        if host_formats {
            expected.insert(0, "dir");
        }
        assert_eq!(names(MNT), expected);
        assert_eq!(names(&format!("{MNT}/guest-dir")), ["mapped", "renamed"]);
        assert_eq!(fs::read(format!("{MNT}/guest-dir/mapped")).unwrap(), mapped());
        let renamed = fs::metadata(format!("{MNT}/guest-dir/renamed")).unwrap();
        assert_eq!(renamed.permissions().mode(), 0o100640);
        assert_eq!(renamed.nlink(), 2);
        assert_eq!(fs::read(format!("{MNT}/symlink")).unwrap(), written());
        assert_eq!(
            fs::read_link(format!("{MNT}/symlink")).unwrap(),
            std::path::Path::new("guest-dir/renamed")
        );
        assert_eq!(fs::read(format!("{MNT}/link")).unwrap(), written());
    }

    /// Writes a file and fsyncs it and its directory, and writes more without, and leaves the
    /// filesystem mounted, for the VM to stop with.
    fn crash() {
        assert_eq!(names(MNT), Vec::<String>::new());
        let mut file = fs::File::create(format!("{MNT}/durable")).unwrap();
        file.write_all(&durable()).unwrap();
        file.sync_all().unwrap();
        fs::File::open(MNT).unwrap().sync_all().unwrap();
        // Not synced: the filesystem may hold them or not, but is consistent either way.
        for i in 0..16 {
            fs::write(format!("{MNT}/pending-{i}"), pattern(100_000, i)).unwrap();
        }
        fs::remove_file(format!("{MNT}/pending-3")).unwrap();
    }

    /// The names in `dir`, sorted.
    fn names(dir: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    impl Test for TestFxfsGuest {
        fn in_guest(self: Box<Self>) {
            fs::create_dir_all(MNT).unwrap();
            // A tag no device has.
            assert_eq!(
                mount("missing").unwrap_err().raw_os_error(),
                Some(libc::ENOENT)
            );
            mount("extents").unwrap();
            if self.mode == Mode::Crash {
                crash();
                println!("OK");
                return;
            }
            let host_formats = self.mode == Mode::HostFormats;
            if host_formats {
                read_hosts();
            } else {
                assert_eq!(names(MNT), Vec::<String>::new());
            }
            assert_eq!(
                fs::metadata(format!("{MNT}/missing")).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );

            write(&format!("{MNT}/guest-dir"));

            // The store mounted again, elsewhere, is the same filesystem.
            const MNT2: &str = "/mnt/fxfs2";
            fs::create_dir_all(MNT2).unwrap();
            mount_at("extents", MNT2).unwrap();
            assert_eq!(names(MNT2), names(MNT));
            fs::write(format!("{MNT2}/shared"), b"through the second mount").unwrap();
            assert_eq!(
                fs::read(format!("{MNT}/shared")).unwrap(),
                b"through the second mount"
            );
            fs::remove_file(format!("{MNT}/shared")).unwrap();
            unmount_at(MNT2);

            unmount();
            mount("extents").unwrap();
            check_written(host_formats);
            unmount();

            println!("OK");
        }
    }
}
