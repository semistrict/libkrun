// Test resizing an extent store while the guest kernel's Fxfs has it mounted
// (fxdax's TASK-37).
//
// The host boots the kernel fxdax's scripts/build-guest-kernel.sh builds, which
// FXDAX_GUEST_KERNEL names, with an extent-store device on a new store, which
// the guest formats as it mounts it. Then, step by step, as the guest asks by
// creating a file in its root (the host's directory, shared), the host resizes
// the store through fxdax's extent daemon, as fxdaxctl resize does, and
// answers with another file: it grows the store, which the guest's statfs and
// allocator take while mounted, the guest then writing more than the first size
// held; shrinks it, above what the filesystem holds, which the guest agrees to;
// and fails to shrink it below what the store holds. Remounted, the filesystem
// has the size it was resized to, and so has the store, once the VM exits.
//
// Under Firecracker it doesn't run: the guest's root is an image of the host's
// directory, not the directory, so the steps' files don't reach the host.

use macros::{guest, host};

pub struct TestFxfsGuestResize;

const MIB: u64 = 1 << 20;
/// The store's capacity as the VM starts.
#[cfg_attr(feature = "guest", allow(dead_code))]
const CAPACITY: u64 = 64 * MIB;
/// The resizes, in order: each step's name, and the capacity the host resizes the store to.
#[cfg_attr(feature = "guest", allow(dead_code))]
const STEPS: [(&str, u64); 3] = [("grow", 256 * MIB), ("shrink", 200 * MIB), ("too-small", 32 * MIB)];

#[host]
mod host {
    use super::*;

    use crate::common::{fxdax_guest_should_run, setup_rootfs};
    use crate::vm::{Vm, Vmm};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Waits for `path` to exist, as long as a step of the guest's takes at most.
    fn wait_for(path: &Path) -> bool {
        let start = Instant::now();
        while !path.exists() {
            if start.elapsed() > Duration::from_secs(120) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// Resizes the store at `store` through the daemon at `extentd` as the guest, whose root is
    /// `root`, asks at each step, and answers with the outcome: `ok`, or the errno's number.
    fn resize_steps(root: &Path, store: &Path, extentd: &Path) {
        for (step, capacity) in STEPS {
            if !wait_for(&root.join(format!("resize-{step}"))) {
                return;
            }
            let outcome = match fxdax::extentd::resize(extentd, store, capacity) {
                Ok(()) => "ok".to_owned(),
                Err(err) => err.errno().map_or("no errno".to_owned(), |errno| errno.raw().to_string()),
            };
            std::fs::write(root.join(format!("resized-{step}")), outcome).unwrap();
        }
    }

    impl Test for TestFxfsGuestResize {
        fn should_run(&self) -> ShouldRun {
            if Vmm::from_env() == Vmm::Firecracker {
                return ShouldRun::No(
                    "the guest's root is an image under Firecracker, which the steps can't share",
                );
            }
            fxdax_guest_should_run()
        }

        /// The guest writes 112 MiB, in steps the host takes.
        fn timeout_secs(&self) -> u64 {
            60
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let store = test_setup.tmp_dir.join("extents");
            let root = setup_rootfs(&test_setup)?;
            let extentd = std::path::PathBuf::from(
                std::env::var_os("FXDAX_EXTENTD").expect("FXDAX_EXTENTD names the daemon"),
            );
            let mut vm = Vm::new(2, 512)?;
            vm.guest_kernel()?;
            vm.extent_store("extents", &store, CAPACITY, 0)?;
            let (resizing_root, resizing_store) = (root.clone(), store.clone());
            std::thread::spawn(move || resize_steps(&resizing_root, &resizing_store, &extentd));
            vm.enter(&root, &test_setup.test_case)
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            if output != "OK\n" {
                return TestOutcome::Fail(format!("expected exactly \"OK\\n\", got {output:?}"));
            }
            let store = fxdax::ExtentStore::open(test_setup.tmp_dir.join("extents"))
                .expect("open the extent store");
            assert_eq!(store.info().unwrap().capacity, STEPS[1].1);
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
    use std::io::Write;
    use std::time::{Duration, Instant};

    const MNT: &str = "/mnt/fxfs";

    fn mount() {
        let source = CString::new("extents").unwrap();
        let target = CString::new(MNT).unwrap();
        let fstype = CString::new("fxfs").unwrap();
        let ret = unsafe {
            libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null())
        };
        assert_eq!(ret, 0, "mount: {}", std::io::Error::last_os_error());
    }

    fn unmount() {
        let target = CString::new(MNT).unwrap();
        assert_eq!(unsafe { libc::umount(target.as_ptr()) }, 0, "umount");
    }

    /// The filesystem's size, as statfs says.
    fn size() -> u64 {
        let target = CString::new(MNT).unwrap();
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::statvfs(target.as_ptr(), &mut stat) }, 0, "statvfs");
        stat.f_blocks * stat.f_frsize
    }

    /// Waits for statfs to say the filesystem's size is `expected`: the device's configuration
    /// change reaches the filesystem soon after the host resizes the store.
    fn wait_for_size(expected: u64) {
        let start = Instant::now();
        while size() != expected {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the size is {}, not {expected}",
                size()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Writes `len` bytes to a new file at `path`, durably.
    fn write(path: &str, len: u64) -> std::io::Result<()> {
        let mut file = fs::File::create(path)?;
        let chunk: Vec<u8> = (0..MIB).map(|i| (i % 251) as u8).collect();
        for _ in 0..len / MIB {
            file.write_all(&chunk)?;
        }
        file.sync_all()
    }

    /// Asks the host to take `step`, and returns its outcome.
    fn resize(step: &str) -> String {
        fs::write(format!("/resize-{step}"), b"").unwrap();
        let answer = format!("/resized-{step}");
        let start = Instant::now();
        loop {
            if let Ok(outcome) = fs::read_to_string(&answer) {
                return outcome;
            }
            assert!(start.elapsed() < Duration::from_secs(60), "no answer to {step}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    impl Test for TestFxfsGuestResize {
        fn in_guest(self: Box<Self>) {
            fs::write("/proc/sys/kernel/printk", b"5").unwrap();
            fs::create_dir_all(MNT).unwrap();
            mount();
            assert_eq!(size(), CAPACITY);
            write(&format!("{MNT}/first"), 16 * MIB).unwrap();

            // Grown, the filesystem takes more than the first size held.
            let (step, grown) = STEPS[0];
            assert_eq!(resize(step), "ok");
            wait_for_size(grown);
            write(&format!("{MNT}/second"), 96 * MIB).unwrap();

            // Shrunk above what it holds, which the guest agrees to.
            let (step, shrunk) = STEPS[1];
            assert_eq!(resize(step), "ok");
            wait_for_size(shrunk);

            // Not below what the store holds.
            let (step, _) = STEPS[2];
            assert_eq!(resize(step), libc::ENOSPC.to_string());
            assert_eq!(size(), shrunk);

            // Remounted, it has the size it was resized to, and what it held.
            unmount();
            mount();
            assert_eq!(size(), shrunk);
            assert_eq!(fs::metadata(format!("{MNT}/second")).unwrap().len(), 96 * MIB);
            unmount();
            println!("OK");
        }
    }
}
