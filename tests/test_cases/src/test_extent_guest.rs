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

    use crate::common::{
        add_extent_store, fxdax_guest_should_run, set_guest_kernel, setup_fs_and_enter,
    };
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
    use krun_sys::*;

    impl Test for TestExtentGuest {
        fn should_run(&self) -> ShouldRun {
            fxdax_guest_should_run()
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let store = test_setup.tmp_dir.join("extents");
            unsafe {
                krun_call!(krun_set_log_level(KRUN_LOG_LEVEL_WARN))?;
                let ctx = krun_call_u32!(krun_create_ctx())?;
                krun_call!(krun_set_vm_config(ctx, 2, 512))?;
                set_guest_kernel(ctx)?;
                add_extent_store(ctx, c"extents", &store, 64 << 20, 64 << 20)?;
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
