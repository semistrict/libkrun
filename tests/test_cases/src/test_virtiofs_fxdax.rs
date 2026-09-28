// Test the fxdax virtio-fs backend, with DAX.
//
// The host creates an fxdax image and exposes it with krun_add_virtiofs_fxdax.
// The guest mounts it with dax=always, checks that files really are
// DAX-mapped, and exercises writes through mappings, regular writes and
// namespace operations, and evicts a file to remove its mappings. After the VM
// exits the host reopens the image, checks it, and checks what the guest left
// behind.

use macros::{guest, host};

pub struct TestVirtiofsFxdax;

/// Offset of the second 2 MiB DAX chunk.
const CHUNK: usize = 2 << 20;
const MAPPED_SIZE: usize = CHUNK + (1 << 20);
const HOST_CONTENT: &[u8] = b"written by the host\n";
const WRITTEN: &[u8] = b"written through FUSE_WRITE\n";
const EVICTED: &[u8] = b"written back by FUSE_REMOVEMAPPING\n";
/// Size of the image, which is sparse.
const IMAGE_SIZE: u64 = 256 << 20;

/// Content of the guest's mmapped file at `offset`.
fn pattern(offset: usize) -> u8 {
    (offset % 251) as u8
}

#[host]
mod host {
    use super::*;

    use crate::common::setup_rootfs;
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
    use fxdax::Vfs;
    use fxdax::vfs::{Attr, Creds, ROOT_INO};
    use krun_sys::*;
    use nix::libc;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::null;

    type KrunAddVirtiofsFxdaxFn = unsafe extern "C" fn(
        ctx_id: u32,
        c_tag: *const std::ffi::c_char,
        c_image_path: *const std::ffi::c_char,
        shm_size: u64,
    ) -> i32;

    fn get_krun_add_virtiofs_fxdax() -> KrunAddVirtiofsFxdaxFn {
        let symbol = CString::new("krun_add_virtiofs_fxdax").unwrap();
        let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr()) };
        assert!(!ptr.is_null(), "krun_add_virtiofs_fxdax not found");
        unsafe { std::mem::transmute(ptr) }
    }

    fn create_image(path: &Path) -> anyhow::Result<()> {
        std::fs::File::create(path)?.set_len(IMAGE_SIZE)?;
        let vfs = Vfs::format_image(path)?;
        let creds = Creds { uid: 0, gid: 0 };
        let file = vfs.create(creds, ROOT_INO, b"from-host", 0o644)?;
        vfs.write(file.ino, 0, HOST_CONTENT)?;
        vfs.close()?;
        Ok(())
    }

    /// Checks the image at `path`, and serves it.
    fn open_image(path: &Path) -> Vfs {
        let vfs = Vfs::mount_image(path).expect("mount image");
        Vfs::fsck(vfs.close().expect("close image")).expect("check image");
        Vfs::mount_image(path).expect("mount image")
    }

    fn lookup_path(vfs: &Vfs, path: &str) -> Attr {
        let mut attr = vfs.getattr(ROOT_INO).unwrap();
        for name in path.split('/') {
            attr = vfs
                .lookup(attr.ino, name.as_bytes())
                .unwrap_or_else(|err| panic!("host: lookup {path}: {err}"));
        }
        attr
    }

    fn read_path(vfs: &Vfs, path: &str) -> Vec<u8> {
        let attr = lookup_path(vfs, path);
        let mut buf = vec![0; attr.size as usize];
        let count = vfs.read(attr.ino, 0, &mut buf).unwrap();
        buf.truncate(count);
        buf
    }

    impl Test for TestVirtiofsFxdax {
        fn should_run(&self) -> ShouldRun {
            if unsafe { krun_call_u32!(krun_has_feature(KRUN_FEATURE_FXDAX.into())) }.ok()
                != Some(1)
            {
                return ShouldRun::No("libkrun compiled without FXDAX");
            }
            ShouldRun::Yes
        }

        fn timeout_secs(&self) -> u64 {
            60
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let krun_add_virtiofs_fxdax = get_krun_add_virtiofs_fxdax();

            let root_dir = setup_rootfs(&test_setup)?;
            let image = test_setup.tmp_dir.join("fxdax.img");
            create_image(&image)?;

            let c_root = CString::new(root_dir.as_os_str().as_bytes())?;
            let c_image = CString::new(image.as_os_str().as_bytes())?;
            let test_case = CString::new(test_setup.test_case)?;

            unsafe {
                krun_call!(krun_set_log_level(KRUN_LOG_LEVEL_WARN))?;
                let ctx = krun_call_u32!(krun_create_ctx())?;
                krun_call!(krun_set_vm_config(ctx, 1, 512))?;
                krun_call!(krun_set_root(ctx, c_root.as_ptr()))?;
                krun_call!(krun_add_virtiofs_fxdax(
                    ctx,
                    c"fxdax".as_ptr(),
                    c_image.as_ptr(),
                    1 << 29,
                ))?;
                krun_call!(krun_set_workdir(ctx, c"/".as_ptr()))?;
                let argv = [test_case.as_ptr(), null()];
                let envp = [null()];
                krun_call!(krun_set_exec(
                    ctx,
                    c"/guest-agent".as_ptr(),
                    argv.as_ptr(),
                    envp.as_ptr(),
                ))?;
                krun_call!(krun_start_enter(ctx))?;
            }
            unreachable!()
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            if output != "OK\n" {
                return TestOutcome::Fail(format!("expected exactly \"OK\\n\", got {output:?}"));
            }

            let vfs = open_image(&test_setup.tmp_dir.join("fxdax.img"));

            let mapped = read_path(&vfs, "dax-mapped");
            assert_eq!(mapped.len(), MAPPED_SIZE, "host: dax-mapped size");
            for (offset, &byte) in mapped.iter().enumerate() {
                let expected = if (CHUNK..CHUNK + WRITTEN.len()).contains(&offset) {
                    WRITTEN[offset - CHUNK]
                } else {
                    pattern(offset)
                };
                assert_eq!(byte, expected, "host: dax-mapped byte {offset}");
            }

            assert_eq!(read_path(&vfs, "written"), WRITTEN);
            assert_eq!(read_path(&vfs, "dir/renamed"), b"renamed");
            assert_eq!(lookup_path(&vfs, "dir/renamed").nlink, 2);
            assert_eq!(read_path(&vfs, "dir/link"), b"renamed");
            assert_eq!(
                vfs.readlink(lookup_path(&vfs, "sym").ino).unwrap(),
                b"written"
            );
            assert_eq!(read_path(&vfs, "truncated"), vec![b't'; 100]);
            assert!(vfs.lookup(ROOT_INO, b"unlinked").is_err());
            // Never fsynced: only removing the mapping wrote it back
            assert_eq!(read_path(&vfs, "evicted"), EVICTED);

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
    use std::panic::catch_unwind;

    const MNT: &str = "/mnt/fxdax";

    fn path(name: &str) -> String {
        format!("{MNT}/{name}")
    }

    fn mount() {
        fs::create_dir_all(MNT).unwrap();
        let source = CString::new("fxdax").unwrap();
        let target = CString::new(MNT).unwrap();
        let fstype = CString::new("virtiofs").unwrap();
        let options = CString::new("dax=always").unwrap();
        let ret = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                0,
                options.as_ptr().cast(),
            )
        };
        assert_eq!(ret, 0, "mount: {}", std::io::Error::last_os_error());
    }

    /// Whether the kernel accesses `path` through the DAX window.
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

    fn test_mmap_write_through_dax() {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path("dax-mapped"))
            .unwrap();
        file.set_len(MAPPED_SIZE as u64).unwrap();
        assert!(is_dax(&path("dax-mapped")), "dax-mapped is not DAX");

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

        // Evicting the inode makes the kernel remove its DAX mappings
        fs::write("/proc/sys/vm/drop_caches", b"2").unwrap();
        assert_eq!(fs::read(path("evicted")).unwrap(), EVICTED);
    }

    impl Test for TestVirtiofsFxdax {
        fn in_guest(self: Box<Self>) {
            mount();

            let tests: &[(&str, fn())] = &[
                ("host_file", test_host_file),
                ("mmap_write_through_dax", test_mmap_write_through_dax),
                ("regular_write", test_regular_write),
                ("namespace", test_namespace),
                ("truncate_mapped", test_truncate_mapped),
                ("eviction_writes_back", test_eviction_writes_back),
            ];
            let mut failed = Vec::new();
            for (name, test) in tests {
                if catch_unwind(test).is_err() {
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
