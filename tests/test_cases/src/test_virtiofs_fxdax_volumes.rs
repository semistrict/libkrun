// Test two virtio-fs devices serving two volumes of one fxdax image.
//
// The host formats an empty fxdax image and adds two devices with
// krun_add_virtiofs_fxdax_volume, for volumes "a" and "b", which the first
// device to open the image creates. The guest mounts both, and checks that
// each is a filesystem of its own. After the VM exits the host reopens the
// image, checks it, and checks what the guest left in each volume.

use macros::{guest, host};

pub struct TestVirtiofsFxdaxVolumes;

const IN_A: &[u8] = b"written to volume a\n";
const IN_B: &[u8] = b"written to volume b\n";
/// Size of the image, which is sparse.
#[cfg_attr(feature = "guest", allow(dead_code))]
const IMAGE_SIZE: u64 = 256 << 20;

#[host]
mod host {
    use super::*;

    use crate::common::setup_rootfs;
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
    use futures::executor::block_on;
    use fxdax::{Filesystem, Image, Inode, Storage};
    use krun_sys::*;
    use nix::libc;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::ptr::null;

    type KrunAddVirtiofsFxdaxVolumeFn = unsafe extern "C" fn(
        ctx_id: u32,
        c_tag: *const std::ffi::c_char,
        c_image_path: *const std::ffi::c_char,
        c_volume: *const std::ffi::c_char,
        shm_size: u64,
    ) -> i32;

    fn get_krun_add_virtiofs_fxdax_volume() -> KrunAddVirtiofsFxdaxVolumeFn {
        let symbol = CString::new("krun_add_virtiofs_fxdax_volume").unwrap();
        let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr()) };
        assert!(!ptr.is_null(), "krun_add_virtiofs_fxdax_volume not found");
        unsafe { std::mem::transmute(ptr) }
    }

    /// The contents of `name` in the root of `fs`, if it has one.
    fn read_file(fs: &Filesystem, name: &[u8]) -> Option<Vec<u8>> {
        let attr = block_on(fs.lookup(Inode::ROOT, name)).ok()?;
        let mut buf = vec![0; attr.size as usize];
        let count = block_on(fs.read(attr.inode, 0, &mut buf)).unwrap();
        buf.truncate(count);
        Some(buf)
    }

    impl Test for TestVirtiofsFxdaxVolumes {
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
            let krun_add_virtiofs_fxdax_volume = get_krun_add_virtiofs_fxdax_volume();

            let root_dir = setup_rootfs(&test_setup)?;
            let image = test_setup.tmp_dir.join("fxdax.img");
            block_on(async {
                Image::format(Storage::create(&image, IMAGE_SIZE)?)
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
                krun_call!(krun_add_virtiofs_fxdax_volume(
                    ctx,
                    c"fxdax-a".as_ptr(),
                    c_image.as_ptr(),
                    c"a".as_ptr(),
                    1 << 29,
                ))?;
                krun_call!(krun_add_virtiofs_fxdax_volume(
                    ctx,
                    c"fxdax-b".as_ptr(),
                    c_image.as_ptr(),
                    c"b".as_ptr(),
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

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            if output != "OK\n" {
                return TestOutcome::Fail(format!("expected exactly \"OK\\n\", got {output:?}"));
            }

            let storage = Storage::open(test_setup.tmp_dir.join("fxdax.img")).expect("open image");
            let image = block_on(async { Image::mount(storage.check().await?).await })
                .expect("mount image");
            assert_eq!(block_on(image.volumes()).unwrap(), ["a", "b"]);
            let a = block_on(image.open_volume("a")).unwrap();
            let b = block_on(image.open_volume("b")).unwrap();
            assert_eq!(read_file(&a, b"file").as_deref(), Some(IN_A));
            assert_eq!(read_file(&b, b"file").as_deref(), Some(IN_B));
            assert_eq!(read_file(&a, b"only-in-a").as_deref(), Some(&b""[..]));
            assert_eq!(read_file(&b, b"only-in-a"), None);
            drop((a, b));
            block_on(image.close()).expect("close image");

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

    fn mount(tag: &str, target: &str, options: &str) {
        fs::create_dir_all(target).unwrap();
        let source = CString::new(tag).unwrap();
        let c_target = CString::new(target).unwrap();
        let fstype = CString::new("virtiofs").unwrap();
        let options = CString::new(options).unwrap();
        let ret = unsafe {
            libc::mount(
                source.as_ptr(),
                c_target.as_ptr(),
                fstype.as_ptr(),
                0,
                options.as_ptr().cast(),
            )
        };
        assert_eq!(ret, 0, "mount {tag}: {}", std::io::Error::last_os_error());
    }

    impl Test for TestVirtiofsFxdaxVolumes {
        fn in_guest(self: Box<Self>) {
            mount("fxdax-a", "/mnt/a", "dax=always");
            mount("fxdax-b", "/mnt/b", "");

            fs::write("/mnt/a/file", IN_A).unwrap();
            fs::write("/mnt/a/only-in-a", b"").unwrap();
            assert!(!fs::exists("/mnt/b/only-in-a").unwrap(), "a's file is in b");
            fs::write("/mnt/b/file", IN_B).unwrap();
            assert_eq!(fs::read("/mnt/a/file").unwrap(), IN_A);
            assert_eq!(fs::read("/mnt/b/file").unwrap(), IN_B);

            // The VMM exits without unmounting, so what isn't synced is lost.
            for path in [
                "/mnt/a/file",
                "/mnt/a/only-in-a",
                "/mnt/b/file",
                "/mnt/a",
                "/mnt/b",
            ] {
                fs::File::open(path).unwrap().sync_all().unwrap();
            }
            println!("OK");
        }
    }
}
