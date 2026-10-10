// Test sizing fxdax's in-process kernel memory with krun_set_fxdax_memory.
//
// The host gives fxdax's kernel more than its default 1 GiB, once (setting it
// again fails), and exposes an fxdax image without DAX. The guest writes a
// file larger than the default, drops its own caches, and reads it back.

use macros::{guest, host};

pub struct TestVirtiofsFxdaxMemory;

const MIB: u64 = 1 << 20;
/// What fxdax's kernel is given: more than its default 1 GiB.
#[cfg_attr(feature = "guest", allow(dead_code))]
const KERNEL_MEMORY: u64 = 1536 * MIB;
/// The file the guest writes and reads back: larger than the default.
const FILE_SIZE: u64 = 1280 * MIB;
/// Size of the image, which is sparse.
#[cfg_attr(feature = "guest", allow(dead_code))]
const IMAGE_SIZE: u64 = 2048 * MIB;

/// The content of the file's `index`th MiB: its index, and then a byte of it.
#[cfg_attr(feature = "host", allow(dead_code))]
fn chunk(index: u64) -> Vec<u8> {
    let mut chunk = vec![(index % 251) as u8; MIB as usize];
    chunk[..8].copy_from_slice(&index.to_le_bytes());
    chunk
}

#[host]
mod host {
    use super::*;

    use crate::common::setup_rootfs;
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
    use futures::executor::block_on;
    use fxdax::{Filesystem, Storage};
    use krun_sys::*;
    use nix::libc;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::ptr::null;

    type KrunAddVirtiofsFxdaxFn = unsafe extern "C" fn(
        ctx_id: u32,
        c_tag: *const std::ffi::c_char,
        c_image_path: *const std::ffi::c_char,
        shm_size: u64,
    ) -> i32;
    type KrunSetFxdaxMemoryFn = unsafe extern "C" fn(memory_bytes: u64) -> i32;

    /// libkrun's function `name`, which only a build with FXDAX=1 has.
    fn symbol<F>(name: &str) -> F {
        let symbol = CString::new(name).unwrap();
        let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr()) };
        assert!(!ptr.is_null(), "{name} not found");
        assert_eq!(size_of::<F>(), size_of::<*mut libc::c_void>());
        unsafe { std::mem::transmute_copy(&ptr) }
    }

    impl Test for TestVirtiofsFxdaxMemory {
        fn should_run(&self) -> ShouldRun {
            if unsafe { krun_call_u32!(krun_has_feature(KRUN_FEATURE_FXDAX.into())) }.ok()
                != Some(1)
            {
                return ShouldRun::No("libkrun compiled without FXDAX");
            }
            ShouldRun::Yes
        }

        fn timeout_secs(&self) -> u64 {
            180
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let krun_add_virtiofs_fxdax: KrunAddVirtiofsFxdaxFn = symbol("krun_add_virtiofs_fxdax");
            let krun_set_fxdax_memory: KrunSetFxdaxMemoryFn = symbol("krun_set_fxdax_memory");

            // Once.
            assert_eq!(unsafe { krun_set_fxdax_memory(KERNEL_MEMORY) }, 0);
            assert_eq!(
                unsafe { krun_set_fxdax_memory(KERNEL_MEMORY) },
                -libc::EBUSY
            );

            let root_dir = setup_rootfs(&test_setup)?;
            let image = test_setup.tmp_dir.join("fxdax.img");
            block_on(async {
                Filesystem::format(Storage::create(&image, IMAGE_SIZE)?)
                    .await?
                    .close()
                    .await
            })?;

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
                    0,
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
    use std::io::{Read, Write};

    const MNT: &str = "/mnt/fxdax";

    fn mount() {
        fs::create_dir_all(MNT).unwrap();
        let source = CString::new("fxdax").unwrap();
        let target = CString::new(MNT).unwrap();
        let fstype = CString::new("virtiofs").unwrap();
        let options = CString::new("dax=never").unwrap();
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

    impl Test for TestVirtiofsFxdaxMemory {
        fn in_guest(self: Box<Self>) {
            mount();
            let path = format!("{MNT}/large");

            let mut file = fs::File::create(&path).unwrap();
            for index in 0..FILE_SIZE / MIB {
                file.write_all(&chunk(index)).unwrap();
            }
            file.sync_all().unwrap();
            drop(file);

            // Read back from fxdax, not the guest's own cache.
            fs::write("/proc/sys/vm/drop_caches", b"3").unwrap();
            let mut file = fs::File::open(&path).unwrap();
            assert_eq!(file.metadata().unwrap().len(), FILE_SIZE);
            let mut buf = vec![0; MIB as usize];
            for index in 0..FILE_SIZE / MIB {
                file.read_exact(&mut buf).unwrap();
                assert!(buf == chunk(index), "MiB {index} differs");
            }

            println!("OK");
        }
    }
}
