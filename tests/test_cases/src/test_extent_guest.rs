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

    use crate::common::{fxdax_guest_should_run, setup_rootfs};
    use crate::vm::Vm;
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};

    impl Test for TestExtentGuest {
        fn should_run(&self) -> ShouldRun {
            fxdax_guest_should_run()
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let store = test_setup.tmp_dir.join("extents");
            let mut vm = Vm::new(2, 512)?;
            vm.guest_kernel()?;
            vm.extent_store("extents", &store, 64 << 20, 64 << 20)?;
            vm.enter(&setup_rootfs(&test_setup)?, &test_setup.test_case)
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
