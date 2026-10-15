// Checks of the guest kernel's own Fxfs (fxfs-guest-checks), against what
// fxdax's own Fxfs on the host sees.
//
// The host creates an fxdax image, an extent store, which the VM gets as an
// extent-store device, served by fxdax's extent daemon, and boots fxdax's
// guest kernel (FXDAX_GUEST_KERNEL), which mounts it with its Fxfs;
// fxfs-guest-checks-dax with a DAX window, which Fxfs maps files' data from
// (on hosts whose pages are 4 KiB, Fxfs's blocks). The guest checks whether
// files are DAX-mapped, and exercises writes through mappings, regular writes
// and namespace operations, and evicts a file to write it back. After the VM
// exits the host reopens the image, checks it, and checks what the guest left
// behind.

use macros::{guest, host};

pub struct TestFxfsGuestChecks {
    /// Whether the device has a DAX window, and the guest maps files into it.
    pub dax: bool,
}

/// Offset of the second 2 MiB DAX chunk.
const CHUNK: usize = 2 << 20;
const MAPPED_SIZE: usize = CHUNK + (1 << 20);
const HOST_CONTENT: &[u8] = b"written by the host\n";
const WRITTEN: &[u8] = b"written through FUSE_WRITE\n";
const EVICTED: &[u8] = b"written back when its inode is evicted\n";
/// Size of the image, which is sparse.
#[cfg_attr(feature = "guest", allow(dead_code))]
const IMAGE_SIZE: u64 = 256 << 20;

/// Content of the guest's mmapped file at `offset`.
fn pattern(offset: usize) -> u8 {
    (offset % 251) as u8
}

#[host]
mod host {
    use super::*;

    use crate::common::{
        add_extent_store, fxdax_guest_should_run, set_guest_kernel, setup_fs_and_enter,
    };
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
    use futures::executor::block_on;
    use fxdax::{Attr, Filesystem, Inode, Owner, Storage};
    use krun_sys::*;
    use std::path::Path;

    fn create_image(path: &Path) -> anyhow::Result<()> {
        block_on(async {
            let fs = Filesystem::format(Storage::create(path, IMAGE_SIZE)?).await?;
            let owner = Owner { uid: 0, gid: 0 };
            let file = fs.create(Inode::ROOT, b"from-host", 0o644, owner).await?;
            fs.write(file.inode, 0, HOST_CONTENT).await?;
            fs.close().await?;
            Ok(())
        })
    }

    /// Checks the image at `path`, and serves it.
    fn open_image(path: &Path) -> Filesystem {
        let storage = Storage::open(path).expect("open image");
        block_on(async { Filesystem::mount(storage.check().await?).await }).expect("mount image")
    }

    fn lookup_path(fs: &Filesystem, path: &str) -> Attr {
        let mut attr = block_on(fs.getattr(Inode::ROOT)).unwrap();
        for name in path.split('/') {
            attr = block_on(fs.lookup(attr.inode, name.as_bytes()))
                .unwrap_or_else(|err| panic!("host: lookup {path}: {err}"));
        }
        attr
    }

    fn read_path(fs: &Filesystem, path: &str) -> Vec<u8> {
        let attr = lookup_path(fs, path);
        let mut buf = vec![0; attr.size as usize];
        let count = block_on(fs.read(attr.inode, 0, &mut buf)).unwrap();
        buf.truncate(count);
        buf
    }

    impl Test for TestFxfsGuestChecks {
        fn should_run(&self) -> ShouldRun {
            fxdax_guest_should_run()
        }

        fn timeout_secs(&self) -> u64 {
            60
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let image = test_setup.tmp_dir.join("fxdax.img");
            create_image(&image)?;
            unsafe {
                krun_call!(krun_set_log_level(KRUN_LOG_LEVEL_WARN))?;
                let ctx = krun_call_u32!(krun_create_ctx())?;
                krun_call!(krun_set_vm_config(ctx, 2, 512))?;
                set_guest_kernel(ctx)?;
                let window = if self.dax { 1 << 29 } else { 0 };
                add_extent_store(ctx, c"fxdax", &image, IMAGE_SIZE, window)?;
                setup_fs_and_enter(ctx, test_setup)?;
            }
            unreachable!()
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            if output != "OK\n" {
                return TestOutcome::Fail(format!("expected exactly \"OK\\n\", got {output:?}"));
            }

            let fs = open_image(&test_setup.tmp_dir.join("fxdax.img"));

            let mapped = read_path(&fs, "dax-mapped");
            assert_eq!(mapped.len(), MAPPED_SIZE, "host: dax-mapped size");
            // The guest's Fxfs, with a window, wrote more once the file was read in as DAX.
            let dax_writes = self.dax;
            let after = CHUNK + 4096;
            for (offset, &byte) in mapped.iter().enumerate() {
                let expected = if dax_writes && offset < EVICTED.len() {
                    EVICTED[offset]
                } else if dax_writes && (after..after + WRITTEN.len()).contains(&offset) {
                    WRITTEN[offset - after]
                } else if (CHUNK..CHUNK + WRITTEN.len()).contains(&offset) {
                    WRITTEN[offset - CHUNK]
                } else {
                    pattern(offset)
                };
                assert_eq!(byte, expected, "host: dax-mapped byte {offset}");
            }

            assert_eq!(read_path(&fs, "written"), WRITTEN);
            assert_eq!(read_path(&fs, "dir/renamed"), b"renamed");
            assert_eq!(lookup_path(&fs, "dir/renamed").nlink, 2);
            assert_eq!(read_path(&fs, "dir/link"), b"renamed");
            assert_eq!(
                block_on(fs.readlink(lookup_path(&fs, "sym").inode)).unwrap(),
                b"written"
            );
            assert_eq!(read_path(&fs, "truncated"), vec![b't'; 100]);
            assert!(block_on(fs.lookup(Inode::ROOT, b"unlinked")).is_err());
            // Never fsynced: only removing the mapping wrote it back
            assert_eq!(read_path(&fs, "evicted"), EVICTED);

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
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    const MNT: &str = "/mnt/fxdax";

    fn path(name: &str) -> String {
        format!("{MNT}/{name}")
    }

    /// Mounts the image with the kernel's Fxfs.
    fn mount() {
        fs::create_dir_all(MNT).unwrap();
        let source = CString::new("fxdax").unwrap();
        let target = CString::new(MNT).unwrap();
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
        assert_eq!(ret, 0, "mount: {}", std::io::Error::last_os_error());
    }

    /// Whether the kernel accesses `path` through the DAX window.
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

    fn is_dax(path: &str) -> bool {
        const STATX_ATTR_DAX: u64 = 0x200000;
        // struct statx: stx_attributes at byte 8, stx_attributes_mask at 56
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
        assert_ne!(mask & STATX_ATTR_DAX, 0, "kernel does not report DAX");
        attributes & STATX_ATTR_DAX != 0
    }

    fn fsync_dir(dir: &str) {
        fs::File::open(dir).unwrap().sync_all().unwrap();
    }

    fn test_host_file() {
        assert_eq!(fs::read(path("from-host")).unwrap(), HOST_CONTENT);
    }

    fn test_mmap_write(dax: bool) {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path("dax-mapped"))
            .unwrap();
        file.set_len(MAPPED_SIZE as u64).unwrap();
        // The guest's Fxfs maps a file's data from the window as it is created; its mappings
        // outlive its inode, which is read in again.
        assert_eq!(is_dax(&path("dax-mapped")), dax, "dax-mapped's DAX attribute");

        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                MAPPED_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            map,
            libc::MAP_FAILED,
            "mmap: {}",
            std::io::Error::last_os_error()
        );
        let bytes = unsafe { std::slice::from_raw_parts_mut(map.cast::<u8>(), MAPPED_SIZE) };
        for (offset, byte) in bytes.iter_mut().enumerate() {
            *byte = pattern(offset);
        }

        // A regular write while mapped is visible through the mapping
        let mut writer = &file;
        writer.seek(SeekFrom::Start(CHUNK as u64)).unwrap();
        writer.write_all(WRITTEN).unwrap();
        assert_eq!(&bytes[CHUNK..CHUNK + WRITTEN.len()], WRITTEN);

        // And writes through the mapping are visible to regular reads
        let mut reader = &file;
        let mut buf = vec![0; 16];
        reader.seek(SeekFrom::Start(100)).unwrap();
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, bytes[100..116]);

        assert_eq!(unsafe { libc::msync(map, MAPPED_SIZE, libc::MS_SYNC) }, 0);
        assert_eq!(unsafe { libc::munmap(map, MAPPED_SIZE) }, 0);
        file.sync_all().unwrap();
        drop(file);
        if dax {
            evict(&path("dax-mapped"));
            assert!(is_dax(&path("dax-mapped")), "dax-mapped isn't DAX once read in again");
            let data = fs::read(path("dax-mapped")).unwrap();
            assert_eq!(&data[CHUNK..CHUNK + WRITTEN.len()], WRITTEN);

            // Writes to it, through a mapping and with write(2), go through the window.
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path("dax-mapped"))
                .unwrap();
            let map = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    MAPPED_SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(map, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
            let bytes = unsafe { std::slice::from_raw_parts_mut(map.cast::<u8>(), MAPPED_SIZE) };
            bytes[..EVICTED.len()].copy_from_slice(EVICTED);
            let mut writer = &file;
            writer.seek(SeekFrom::Start(CHUNK as u64 + 4096)).unwrap();
            writer.write_all(WRITTEN).unwrap();
            assert_eq!(&bytes[CHUNK + 4096..CHUNK + 4096 + WRITTEN.len()], WRITTEN);
            assert_eq!(unsafe { libc::msync(map, MAPPED_SIZE, libc::MS_SYNC) }, 0);
            assert_eq!(unsafe { libc::munmap(map, MAPPED_SIZE) }, 0);
            file.sync_all().unwrap();
            drop(file);
            let data = fs::read(path("dax-mapped")).unwrap();
            assert_eq!(&data[..EVICTED.len()], EVICTED);
            assert_eq!(&data[CHUNK + 4096..CHUNK + 4096 + WRITTEN.len()], WRITTEN);
        }
    }

    fn test_regular_write() {
        let mut file = fs::File::create(path("written")).unwrap();
        file.write_all(WRITTEN).unwrap();
        file.sync_all().unwrap();
        assert_eq!(fs::read(path("written")).unwrap(), WRITTEN);
    }

    fn test_namespace() {
        fs::create_dir(path("dir")).unwrap();
        fs::write(path("dir/tmp"), b"renamed").unwrap();
        fs::rename(path("dir/tmp"), path("dir/renamed")).unwrap();
        fs::hard_link(path("dir/renamed"), path("dir/link")).unwrap();
        assert_eq!(fs::metadata(path("dir/renamed")).unwrap().nlink(), 2);
        std::os::unix::fs::symlink("written", path("sym")).unwrap();
        assert_eq!(fs::read(path("sym")).unwrap(), WRITTEN);

        fs::write(path("unlinked"), b"gone").unwrap();
        fs::remove_file(path("unlinked")).unwrap();

        let mut names: Vec<String> = fs::read_dir(path("dir"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["link", "renamed"]);

        fs::File::open(path("dir/renamed"))
            .unwrap()
            .sync_all()
            .unwrap();
        fsync_dir(&path("dir"));
    }

    fn test_truncate_mapped() {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path("truncated"))
            .unwrap();
        (&file).write_all(&[b't'; 10000]).unwrap();

        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED);
        file.set_len(100).unwrap();
        let page = unsafe { std::slice::from_raw_parts(map.cast::<u8>(), 4096) };
        assert_eq!(page[99], b't');
        assert!(page[100..].iter().all(|&b| b == 0), "tail not zeroed");
        assert_eq!(unsafe { libc::munmap(map, 4096) }, 0);
        file.sync_all().unwrap();
    }

    fn test_eviction_writes_back() {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path("evicted"))
            .unwrap();
        file.set_len(EVICTED.len() as u64).unwrap();
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                EVICTED.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED);
        unsafe { std::slice::from_raw_parts_mut(map.cast::<u8>(), EVICTED.len()) }
            .copy_from_slice(EVICTED);
        assert_eq!(unsafe { libc::munmap(map, EVICTED.len()) }, 0);
        drop(file);

        // Evicting the inode writes it back: with DAX, by removing its mappings
        fs::write("/proc/sys/vm/drop_caches", b"2").unwrap();
        assert_eq!(fs::read(path("evicted")).unwrap(), EVICTED);
    }

    impl Test for TestFxfsGuestChecks {
        fn in_guest(self: Box<Self>) {
            let dax = self.dax;
            mount();

            let mmap_write = || test_mmap_write(dax);
            let tests: &[(&str, &dyn Fn())] = &[
                ("host_file", &test_host_file),
                ("mmap_write", &mmap_write),
                ("regular_write", &test_regular_write),
                ("namespace", &test_namespace),
                ("truncate_mapped", &test_truncate_mapped),
                ("eviction_writes_back", &test_eviction_writes_back),
            ];
            let mut failed = Vec::new();
            for (name, test) in tests {
                if catch_unwind(AssertUnwindSafe(test)).is_err() {
                    eprintln!("FAIL: {name}");
                    failed.push(*name);
                } else {
                    eprintln!("PASS: {name}");
                }
            }
            fsync_dir(MNT);

            if failed.is_empty() {
                println!("OK");
            } else {
                println!("FAILED: {}", failed.join(", "));
            }
        }
    }
}
