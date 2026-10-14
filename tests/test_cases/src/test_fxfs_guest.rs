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
// fxfs-guest-crash: the guest formats the store, fsyncs file after file,
// reporting each once its fsync returns, while another thread does too, and
// resets the VM in the middle, without syncing. The host checks the store's
// filesystem (fsck), and finds every file the guest reported, whole.
// fxfs-guest-dax and fxfs-guest-crash-dax: the same with a DAX window, which
// the guest's Fxfs maps files' data from (on hosts whose pages are 4 KiB,
// Fxfs's blocks): files are DAX, and reading one maps it rather than reading it.

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
    /// Whether the device has a DAX window.
    dax: bool,
}

impl TestFxfsGuest {
    /// fxfs-guest.
    pub fn host_formats() -> Self {
        Self { mode: Mode::HostFormats, dax: false }
    }

    /// fxfs-guest-format.
    pub fn guest_formats() -> Self {
        Self { mode: Mode::GuestFormats, dax: false }
    }

    /// fxfs-guest-crash.
    pub fn crash() -> Self {
        Self { mode: Mode::Crash, dax: false }
    }

    /// The same test with a DAX window.
    pub fn with_dax(self) -> Self {
        Self { dax: true, ..self }
    }
}

/// The DAX window's size, when the device has one.
#[cfg_attr(feature = "guest", allow(dead_code))]
const WINDOW: u64 = 64 << 20;

/// The contents of the crash test's file `index`, of `writer`: of a size and pattern its name
/// gives, which the host checks.
fn durable(writer: u8, index: usize) -> Vec<u8> {
    pattern(4096 * (index % 37) + 13 * index + 1, writer.wrapping_mul(31) ^ index as u8)
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

    use crate::common::{add_extent_store, set_guest_kernel, setup_fs_and_enter};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
    use futures::executor::block_on;
    use fxdax::{Filesystem, Inode, Owner, Storage};
    use krun_sys::*;

    /// The store's capacity.
    const CAPACITY: u64 = 64 << 20;

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

            unsafe {
                krun_call!(krun_set_log_level(KRUN_LOG_LEVEL_WARN))?;
                let ctx = krun_call_u32!(krun_create_ctx())?;
                krun_call!(krun_set_vm_config(ctx, 2, 512))?;
                set_guest_kernel(ctx)?;
                add_extent_store(ctx, c"extents", &path, CAPACITY, if self.dax { WINDOW } else { 0 })?;
                setup_fs_and_enter(ctx, test_setup)?;
            }
            unreachable!()
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            let path = test_setup.tmp_dir.join("extents");
            if self.mode == Mode::Crash {
                // The guest stopped mid-workload: its filesystem is consistent, and holds every
                // file whose fsync returned before it did.
                let synced: Vec<(u8, usize)> = output
                    .lines()
                    .filter_map(|line| line.strip_prefix("synced "))
                    .map(|file| {
                        let (writer, index) = file.split_once(' ').unwrap();
                        (writer.parse().unwrap(), index.parse().unwrap())
                    })
                    .collect();
                if synced.len() < 10 {
                    return TestOutcome::Fail(format!("too few fsyncs returned: {output:?}"));
                }
                let storage = block_on(Storage::open(&path).unwrap().check()).unwrap();
                let fs = block_on(Filesystem::mount(storage)).unwrap();
                for (writer, index) in synced {
                    let name = format!("synced-{writer}-{index}");
                    let file = block_on(fs.lookup(Inode::ROOT, name.as_bytes())).unwrap();
                    let expected = durable(writer, index);
                    assert_eq!(file.size, expected.len() as u64, "{name}");
                    let mut contents = vec![0; expected.len()];
                    let mut read = 0;
                    while read < contents.len() {
                        let n = block_on(fs.read(file.inode, read as u64, &mut contents[read..]))
                            .unwrap();
                        assert_ne!(n, 0, "{name} ends early, at {read}");
                        read += n;
                    }
                    assert!(contents == expected, "{name} holds other data");
                }
                block_on(fs.close()).unwrap();
                return TestOutcome::Pass;
            }
            if output != "OK\n" {
                return TestOutcome::Fail(format!("expected exactly \"OK\\n\", got {output:?}"));
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
    use std::ffi::{CStr, CString};
    use std::fs;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// What the guest writes through a mapping: three pages and a bit.
    fn mapped() -> Vec<u8> {
        pattern(3 * 4096 + 100, 0xa5)
    }

    const MNT: &str = "/mnt/fxfs";

    /// Evicts `path`'s inode, for it to be read in again: written back, and its pages dropped,
    /// as an inode that has any isn't evicted (dropping every cache would drop the filesystem's
    /// metadata's too).
    fn evict(path: &str) {
        let file = fs::File::open(path).unwrap();
        file.sync_all().unwrap();
        let fd = file.as_raw_fd();
        assert_eq!(unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) }, 0);
        drop(file);
        fs::write("/proc/sys/vm/drop_caches", b"2").unwrap();
    }

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
        // And with what the file held written back and dropped from memory in between, so that
        // the zeros come from the filesystem.
        file.sync_all().unwrap();
        file.set_len(2).unwrap();
        file.sync_all().unwrap();
        fs::write("/proc/sys/vm/drop_caches", b"1").unwrap();
        file.set_len(5).unwrap();
        assert_eq!(fs::read(&scratch).unwrap(), b"01\0\0\0");
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
        // A target as long as Linux's PATH_MAX - 1, which Fxfs keeps outside the symlink's
        // record, and one longer, which it refuses.
        let long_target = "a/".repeat(2047) + "z";
        std::os::unix::fs::symlink(&long_target, format!("{dir}/long-link")).unwrap();
        assert_eq!(
            fs::read_link(format!("{dir}/long-link")).unwrap(),
            std::path::Path::new(&long_target)
        );
        assert_eq!(
            std::os::unix::fs::symlink(long_target + "z", format!("{dir}/longer-link"))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENAMETOOLONG)
        );
        fs::remove_file(format!("{dir}/long-link")).unwrap();

        // A directory emptied while it is listed, as `rm -r` empties it: no entry is skipped,
        // though removing those listed before it moves it, and each listing reads several.
        let listed = format!("{dir}/listed");
        fs::create_dir(&listed).unwrap();
        for i in 0..2000 {
            fs::write(format!("{listed}/entry-with-a-long-name-{i}"), b"").unwrap();
        }
        let mut removed = 0;
        for entry in fs::read_dir(&listed).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
            removed += 1;
        }
        assert_eq!(removed, 2000);
        fs::remove_dir(&listed).unwrap();

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

        xattrs(dir);
        exchange(dir);

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

    /// Extended attributes: set, created, replaced, listed, read, removed, and kept.
    fn xattrs(dir: &str) {
        let path = CString::new(format!("{dir}/renamed")).unwrap();
        let set = |name: &CStr, value: &[u8], flags| unsafe {
            libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), flags)
        };
        let get = |name: &CStr| {
            let mut buf = [0u8; 256];
            let len = unsafe {
                libc::getxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
            };
            if len < 0 {
                Err(std::io::Error::last_os_error().raw_os_error().unwrap())
            } else {
                Ok(buf[..len as usize].to_vec())
            }
        };
        assert_eq!(set(c"user.colour", b"blue", 0), 0);
        assert_eq!(get(c"user.colour"), Ok(b"blue".to_vec()));
        // Creating one that exists fails, and replacing one that doesn't.
        assert_eq!(set(c"user.colour", b"red", libc::XATTR_CREATE), -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EEXIST));
        assert_eq!(set(c"user.size", b"large", libc::XATTR_REPLACE), -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ENODATA));
        assert_eq!(set(c"user.colour", b"green", libc::XATTR_REPLACE), 0);
        assert_eq!(set(c"user.size", b"large", libc::XATTR_CREATE), 0);
        assert_eq!(set(c"trusted.kept", b"", 0), 0);
        assert_eq!(get(c"user.colour"), Ok(b"green".to_vec()));
        assert_eq!(get(c"trusted.kept"), Ok(Vec::new()));
        assert_eq!(get(c"user.absent"), Err(libc::ENODATA));

        let mut list = [0u8; 256];
        let len = unsafe { libc::listxattr(path.as_ptr(), list.as_mut_ptr().cast(), list.len()) };
        assert!(len >= 0, "listxattr: {}", std::io::Error::last_os_error());
        let mut names: Vec<&[u8]> = list[..len as usize]
            .split(|&b| b == 0)
            .filter(|name| !name.is_empty())
            .collect();
        names.sort();
        assert_eq!(names, [&b"trusted.kept"[..], b"user.colour", b"user.size"]);

        assert_eq!(unsafe { libc::removexattr(path.as_ptr(), c"user.size".as_ptr()) }, 0);
        assert_eq!(get(c"user.size"), Err(libc::ENODATA));
    }

    /// RENAME_EXCHANGE: a file and a directory swap names, across directories.
    fn exchange(dir: &str) {
        let (file, sub) = (format!("{dir}/swap-file"), format!("{MNT}/swap-dir"));
        fs::write(&file, b"was a file").unwrap();
        fs::create_dir(&sub).unwrap();
        fs::write(format!("{sub}/inside"), b"in the directory").unwrap();
        let (file_c, sub_c) = (CString::new(file.as_str()).unwrap(), CString::new(sub.as_str()).unwrap());
        let ret = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                file_c.as_ptr(),
                libc::AT_FDCWD,
                sub_c.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        };
        assert_eq!(ret, 0, "renameat2: {}", std::io::Error::last_os_error());
        assert_eq!(fs::read(&sub).unwrap(), b"was a file");
        assert_eq!(fs::read(format!("{file}/inside")).unwrap(), b"in the directory");
        // The directories' links follow the directory that moved between them.
        assert_eq!(fs::metadata(dir).unwrap().nlink(), 3);
        fs::remove_file(format!("{file}/inside")).unwrap();
        fs::remove_dir(&file).unwrap();
        fs::remove_file(&sub).unwrap();
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
        let path = CString::new(format!("{MNT}/guest-dir/renamed")).unwrap();
        let mut buf = [0u8; 16];
        let len = unsafe {
            libc::getxattr(path.as_ptr(), c"user.colour".as_ptr(), buf.as_mut_ptr().cast(), 16)
        };
        assert_eq!(&buf[..len.max(0) as usize], b"green");
    }

    /// Writes a file and fsyncs it and its directory, and writes more without, and leaves the
    /// filesystem mounted, for the VM to stop with.
    /// Writes and fsyncs file after file, and the directory, reporting each once both fsyncs
    /// return, until the VM stops.
    fn fsync_files(writer: u8) -> ! {
        for index in 0.. {
            let mut file = fs::File::create(format!("{MNT}/synced-{writer}-{index}")).unwrap();
            file.write_all(&durable(writer, index)).unwrap();
            file.sync_all().unwrap();
            fs::File::open(MNT).unwrap().sync_all().unwrap();
            // Each report a write of its own, as the VM may stop between writes.
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "synced {writer} {index}").unwrap();
            stdout.flush().unwrap();
            // Unsynced changes beside them, which the filesystem may hold or not.
            fs::write(format!("{MNT}/pending-{writer}-{index}"), pattern(5000, writer)).unwrap();
            if index % 3 == 0 {
                let _ = fs::remove_file(format!("{MNT}/pending-{writer}-{}", index / 2));
            }
        }
        unreachable!()
    }

    /// Stops the VM mid-workload: with two writers fsyncing, and without syncing (reboot(2)
    /// doesn't).
    fn crash(dax: bool) -> ! {
        assert_eq!(names(MNT), Vec::<String>::new());
        // A file of 1 MiB or more maps its data from the window once read in again; a small one
        // doesn't.
        let (probe, small) = (format!("{MNT}/probe"), format!("{MNT}/small"));
        fs::write(&probe, pattern(2 << 20, 1)).unwrap();
        fs::write(&small, b"small").unwrap();
        evict(&probe);
        evict(&small);
        assert_eq!(is_dax(&probe), dax, "the DAX attribute");
        assert!(!is_dax(&small), "a small file is DAX");
        fs::remove_file(&probe).unwrap();
        fs::remove_file(&small).unwrap();
        std::thread::spawn(|| fsync_files(1));
        std::thread::spawn(|| fsync_files(2));
        std::thread::sleep(std::time::Duration::from_secs(2));
        // A reset, which ends the VM on every architecture: x86's has no power-off.
        unsafe { libc::reboot(libc::RB_AUTOBOOT) };
        panic!("reboot: {}", std::io::Error::last_os_error());
    }

    /// Whether the kernel accesses `path` through the DAX window: STATX_ATTR_DAX.
    fn is_dax(path: &str) -> bool {
        const STATX_ATTR_DAX: u64 = 0x200000;
        // struct statx: stx_attributes at byte 8, stx_attributes_mask at 56.
        let mut buf = [0u64; 32];
        let c_path = CString::new(path).unwrap();
        let ret = unsafe {
            libc::syscall(
                libc::SYS_statx,
                libc::AT_FDCWD,
                c_path.as_ptr(),
                0,
                0x7ff, // STATX_BASIC_STATS
                buf.as_mut_ptr(),
            )
        };
        assert_eq!(ret, 0, "statx: {}", std::io::Error::last_os_error());
        let (attributes, mask) = (buf[1], buf[7]);
        assert_ne!(mask & STATX_ATTR_DAX, 0, "the kernel does not report DAX");
        attributes & STATX_ATTR_DAX != 0
    }

    /// The device's count of the requests of `kind` (as `device_requests` names them).
    fn requests(kind: &str) -> u64 {
        device_requests()
            .lines()
            .find_map(|line| line.strip_prefix(kind)?.strip_prefix(' ')?.parse().ok())
            .unwrap_or_else(|| panic!("no count of {kind} requests"))
    }

    /// A file's data maps from the window: read in again after its inode is evicted, it takes a
    /// MAP of each of its extents, and no READ. Written back a MiB at a time (FXFS_IO_MAX), 3 MiB
    /// and a bit are in four extents: two in each 2 MiB chunk.
    fn read_maps() {
        let path = format!("{MNT}/mapped-read");
        let data = pattern((3 << 20) + 5, 0x3c);
        fs::write(&path, &data).unwrap();
        evict(&path);
        assert!(is_dax(&path), "{path} is not DAX");
        let (reads, maps) = (requests("read"), requests("map"));
        assert_eq!(fs::read(&path).unwrap(), data);
        assert_eq!(requests("read"), reads, "reading the file read from the device");
        assert_eq!(requests("map"), maps + 4, "the file's extents' MAPs");
        fs::remove_file(&path).unwrap();
    }

    /// The extent-store device's count of the requests it has taken, by type.
    fn device_requests() -> String {
        let devices: Vec<_> = fs::read_dir("/sys/bus/virtio/drivers/virtio_extent")
            .unwrap()
            .map(|entry| entry.unwrap().path().join("requests"))
            .filter(|path| path.exists())
            .collect();
        assert_eq!(devices.len(), 1, "{devices:?}");
        fs::read_to_string(&devices[0]).unwrap()
    }

    /// Lookups, stats, creates, renames and unlinks of metadata in memory take no device requests.
    fn metadata_cached() {
        let dir = format!("{MNT}/cached");
        fs::create_dir(&dir).unwrap();
        // Read in what the operations will touch.
        let _ = names(MNT);
        let _ = fs::metadata(format!("{MNT}/guest-dir/renamed")).unwrap();
        let before = device_requests();
        for i in 0..20 {
            let path = format!("{dir}/file-{i}");
            fs::File::create(&path).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().len(), 0);
            fs::rename(&path, format!("{dir}/moved-{i}")).unwrap();
        }
        assert_eq!(names(&dir).len(), 20);
        let _ = fs::metadata(format!("{MNT}/guest-dir/renamed")).unwrap();
        assert_eq!(
            fs::metadata(format!("{MNT}/guest-dir/absent")).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        for i in 0..20 {
            fs::remove_file(format!("{dir}/moved-{i}")).unwrap();
        }
        assert_eq!(device_requests(), before);
        fs::remove_dir(&dir).unwrap();
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
                crash(self.dax);
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
            // Files are in the page cache as written; read in again, one of 1 MiB or more maps
            // its data from the window.
            let renamed = format!("{MNT}/guest-dir/renamed");
            assert!(!is_dax(&renamed), "a file written is DAX");
            evict(&renamed);
            assert_eq!(is_dax(&renamed), self.dax, "the DAX attribute");
            metadata_cached();
            if self.dax {
                read_maps();
            }

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
