// Test Fxfs in the guest's kernel (fxdax's TASK-26.1 and TASK-26.6).
//
// The host makes an fxdax filesystem on a new extent store, with a file, and a
// directory holding a larger file, and boots the kernel fxdax's
// scripts/build-guest-kernel.sh builds, which FXDAX_GUEST_KERNEL names, with an
// extent-store device on that store. The guest mounts the store's filesystem
// with the kernel's Fxfs, lists it, and reads its files.

use macros::{guest, host};

pub struct TestFxfsGuest;

/// The small file's contents.
#[cfg_attr(feature = "host", allow(dead_code))]
const HELLO: &[u8] = b"Hello from the host's Fxfs\n";

/// The larger file's size: many blocks, and not a whole number of them.
#[cfg_attr(feature = "host", allow(dead_code))]
const NESTED_SIZE: usize = (2 << 20) + 12345;

/// The larger file's contents: a pattern no block repeats.
#[cfg_attr(feature = "host", allow(dead_code))]
fn nested() -> Vec<u8> {
    (0..NESTED_SIZE).map(|i| (i % 251) as u8 ^ (i / 4096) as u8).collect()
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

        fn check(self: Box<Self>, stdout: Vec<u8>, _test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            if output != "OK\n" {
                return TestOutcome::Fail(format!("expected exactly \"OK\\n\", got {output:?}"));
            }
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
    use std::os::unix::fs::PermissionsExt;

    const MNT: &str = "/mnt/fxfs";

    fn mount(source: &str) -> std::io::Result<()> {
        let source = CString::new(source).unwrap();
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
        if ret == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn names(dir: &str) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect()
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

            assert_eq!(
                fs::metadata(format!("{MNT}/missing")).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );

            println!("OK");
        }
    }
}
