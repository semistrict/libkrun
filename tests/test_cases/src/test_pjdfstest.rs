use macros::{guest, host};

pub struct TestPjdfstest {
    /// fxdax's pjdfstest-fxfs-guest: whether to boot fxdax's guest kernel (FXDAX_GUEST_KERNEL)
    /// and test its Fxfs, on an extent-store device on a new store, rather than the root.
    pub guest_fxfs: bool,
}

/// Where the guest mounts its Fxfs.
#[cfg_attr(feature = "host", allow(dead_code))]
const FXFS_MNT: &str = "/mnt/fxfs";

/// pjdfstest's tests Fxfs can't pass, and why: none.
#[cfg_attr(feature = "host", allow(dead_code))]
const FXFS_SKIPPED: &[(&str, &str)] = &[];

#[host]
mod host {
    use super::*;
    use crate::common::{add_extent_store, set_guest_kernel, setup_fs_and_enter_with_env};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
    use krun_sys::*;
    use std::ffi::CString;

    use macros::env_or_default;

    // Set PJDFSTEST_REPO and PJDFSTEST_COMMIT at build time to enable this test.
    const CONTAINERFILE: &str = concat!(
        "FROM fedora:43\n",
        "RUN dnf install -y autoconf automake gcc make perl-Test-Harness git openssl && dnf clean all\n",
        "RUN git init /pjdfstest \\\n",
        " && git -C /pjdfstest fetch --depth 1 ",
        env_or_default!("PJDFSTEST_REPO", ""),
        " ",
        env_or_default!("PJDFSTEST_COMMIT", ""),
        " \\\n",
        " && git -C /pjdfstest checkout FETCH_HEAD\n",
        "WORKDIR /pjdfstest\n",
        "RUN autoreconf -ifs && ./configure && make pjdfstest\n",
    );

    impl Test for TestPjdfstest {
        fn should_run(&self) -> ShouldRun {
            if option_env!("PJDFSTEST_REPO").is_none() || option_env!("PJDFSTEST_COMMIT").is_none()
            {
                return ShouldRun::No("PJDFSTEST_REPO/PJDFSTEST_COMMIT not set");
            }
            if self.guest_fxfs && std::env::var_os("FXDAX_GUEST_KERNEL").is_none() {
                return ShouldRun::No("FXDAX_GUEST_KERNEL does not name a guest kernel");
            }
            ShouldRun::Yes
        }

        fn rootfs_image(&self) -> Option<&'static str> {
            Some(CONTAINERFILE)
        }

        fn timeout_secs(&self) -> u64 {
            1800
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let host_os = if cfg!(target_os = "macos") {
                "Darwin"
            } else {
                "Linux"
            };
            let host_os_env = CString::new(format!("PJDFSTEST_HOST_OS={host_os}"))?;
            unsafe {
                let ctx = krun_call_u32!(krun_create_ctx())?;
                krun_call!(krun_set_vm_config(ctx, 2, 1024))?;
                if self.guest_fxfs {
                    set_guest_kernel(ctx)?;
                    let store = test_setup.tmp_dir.join("extents");
                    add_extent_store(ctx, c"extents", &store, 1 << 30, 0)?;
                }
                setup_fs_and_enter_with_env(ctx, test_setup, &[host_os_env.as_c_str()])?;
            }
            Ok(())
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, _test_setup: TestSetup) -> TestOutcome {
            let stdout = String::from_utf8_lossy(&stdout);

            if stdout.contains("Result: PASS") {
                TestOutcome::Pass
            } else if stdout.contains("Result: FAIL") || stdout.contains("Result: NOTESTS") {
                TestOutcome::Fail(stdout.to_string())
            } else if stdout.trim() == "OK" {
                TestOutcome::Pass
            } else {
                TestOutcome::Fail(stdout.to_string())
            }
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use std::process::Command;

    /// Mounts the guest kernel's Fxfs, from the extent-store device tagged `extents`.
    fn mount_fxfs() {
        std::fs::create_dir_all(FXFS_MNT).unwrap();
        let ret = unsafe {
            nix::libc::mount(
                c"extents".as_ptr(),
                c"/mnt/fxfs".as_ptr(),
                c"fxfs".as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        assert_eq!(ret, 0, "mount: {}", std::io::Error::last_os_error());
    }

    impl Test for TestPjdfstest {
        fn in_guest(self: Box<Self>) {
            // Create a test directory on the filesystem under test
            let work = if self.guest_fxfs {
                mount_fxfs();
                format!("{FXFS_MNT}/pjdfstest-work")
            } else {
                "/tmp/pjdfstest-work".to_owned()
            };
            std::fs::create_dir_all(&work).expect("Failed to create test dir");

            let mut prove = Command::new("/usr/bin/prove");
            prove.arg("-rv");
            if self.guest_fxfs {
                // Each test but those skipped, in the order prove's -r would run them.
                let mut tests = Vec::new();
                for dir in std::fs::read_dir("/pjdfstest/tests").unwrap() {
                    let dir = dir.unwrap().path();
                    if !dir.is_dir() {
                        continue;
                    }
                    for test in std::fs::read_dir(&dir).unwrap() {
                        let test = test.unwrap().path();
                        let name = test.strip_prefix("/pjdfstest/tests").unwrap();
                        let name = name.to_str().unwrap().trim_start_matches('/');
                        if !name.ends_with(".t") {
                            continue;
                        }
                        match FXFS_SKIPPED.iter().find(|(skipped, _)| *skipped == name) {
                            Some((_, why)) => eprintln!("skipped {name}: {why}"),
                            None => tests.push(test),
                        }
                    }
                }
                tests.sort();
                prove.args(tests);
            } else {
                prove.arg("/pjdfstest/tests");
            }
            let status = prove
                .current_dir(&work)
                .status()
                .expect("Failed to run prove");

            if !status.success() {
                panic!("prove exited with status: {}", status.code().unwrap_or(-1));
            }
        }
    }
}
