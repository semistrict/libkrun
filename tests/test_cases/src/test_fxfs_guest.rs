// Test Fxfs in the guest's kernel (fxdax's TASK-26.1).
//
// The host boots the kernel fxdax's scripts/build-guest-kernel.sh builds, which
// FXDAX_GUEST_KERNEL names, instead of libkrunfw's. Its fs/fxfs is the VFS glue
// in C over Rust built outside the kernel's build, which for now serves one
// file, produced by a task on the kernel's workers after a timer. The guest
// mounts it, lists the root and reads the file.

use macros::{guest, host};

pub struct TestFxfsGuest;

#[cfg_attr(feature = "host", allow(dead_code))]
const CONTENTS: &str = "Hello from Rust in the kernel\n";

#[host]
mod host {
    use super::*;

    use crate::common::setup_fs_and_enter;
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
    use krun_sys::*;
    use std::ffi::CString;
    use std::ptr::null;

    /// The format libkrun loads the kernel in: an Image on arm64, an ELF vmlinux on x86-64.
    const KERNEL_FORMAT: u32 = if cfg!(target_arch = "aarch64") {
        KRUN_KERNEL_FORMAT_RAW
    } else {
        KRUN_KERNEL_FORMAT_ELF
    };

    impl Test for TestFxfsGuest {
        fn should_run(&self) -> ShouldRun {
            if std::env::var_os("FXDAX_GUEST_KERNEL").is_none() {
                return ShouldRun::No("FXDAX_GUEST_KERNEL does not name a guest kernel");
            }
            ShouldRun::Yes
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let kernel = CString::new(std::env::var("FXDAX_GUEST_KERNEL")?)?;
            unsafe {
                krun_call!(krun_set_log_level(KRUN_LOG_LEVEL_WARN))?;
                let ctx = krun_call_u32!(krun_create_ctx())?;
                krun_call!(krun_set_vm_config(ctx, 1, 512))?;
                // libkrun's own command line, as with libkrunfw's kernel.
                krun_call!(krun_set_kernel(
                    ctx,
                    kernel.as_ptr(),
                    KERNEL_FORMAT,
                    null(),
                    null()
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

    const MNT: &str = "/mnt/fxfs";

    impl Test for TestFxfsGuest {
        fn in_guest(self: Box<Self>) {
            fs::create_dir_all(MNT).unwrap();
            let source = CString::new("fxfs").unwrap();
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

            let names: Vec<String> = fs::read_dir(MNT)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            assert_eq!(names, ["hello"]);
            assert_eq!(
                fs::read_to_string(format!("{MNT}/hello")).unwrap(),
                CONTENTS
            );
            assert_eq!(
                fs::metadata(format!("{MNT}/missing")).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );

            println!("OK");
        }
    }
}
