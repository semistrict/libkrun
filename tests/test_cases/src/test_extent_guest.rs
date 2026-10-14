// Test the extent-store device and the guest kernel's driver of it (fxdax's
// TASK-26.4 and TASK-26.5).
//
// The host boots the kernel fxdax's scripts/build-guest-kernel.sh builds, which
// FXDAX_GUEST_KERNEL names, with an extent-store device on a new store with a
// DAX window. The guest runs the driver's self-test, which issues every request
// of the protocol, many writes and reads at once, and reads an extent mapped
// into the DAX window in place. After the VM exits the host opens the store and
// checks what the self-test committed.

use macros::{guest, host};

pub struct TestExtentGuest;

/// The root the driver's self-test commits.
#[cfg_attr(feature = "guest", allow(dead_code))]
const ROOT: &[u8] = b"fxdax extent store self-test";

#[host]
mod host {
    use super::*;

    use crate::common::setup_fs_and_enter;
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
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

    impl Test for TestExtentGuest {
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
            let krun_add_extent_store = get_krun_add_extent_store();
            let kernel = CString::new(std::env::var("FXDAX_GUEST_KERNEL")?)?;
            let store = CString::new(test_setup.tmp_dir.join("extents").as_os_str().as_bytes())?;
            unsafe {
                krun_call!(krun_set_log_level(KRUN_LOG_LEVEL_WARN))?;
                let ctx = krun_call_u32!(krun_create_ctx())?;
                krun_call!(krun_set_vm_config(ctx, 2, 512))?;
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
                    64 << 20,
                    64 << 20,
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
            // What the self-test committed is in the store.
            let store = fxdax::ExtentStore::open(test_setup.tmp_dir.join("extents"))
                .expect("open the extent store");
            assert_eq!(store.root().unwrap(), ROOT);
            let info = store.info().unwrap();
            // 33 extents of two blocks written, of which one was freed.
            assert_eq!(info.used, 32 * 2 * 4096);
            assert_eq!(info.next_sequence, 33);
            TestOutcome::Pass
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    use std::fs;

    const DRIVER: &str = "/sys/bus/virtio/drivers/virtio_extent";

    impl Test for TestExtentGuest {
        fn in_guest(self: Box<Self>) {
            let devices: Vec<_> = fs::read_dir(DRIVER)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.join("selftest").exists())
                .collect();
            assert_eq!(devices.len(), 1, "{devices:?}");
            let selftest = devices[0].join("selftest");
            assert_eq!(fs::read_to_string(&selftest).unwrap(), "not run\n");
            fs::write(&selftest, "1").unwrap();
            assert_eq!(fs::read_to_string(&selftest).unwrap(), "ok\n");
            let _ = ROOT;
            println!("OK");
        }
    }
}
