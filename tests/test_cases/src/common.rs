//! Common utilities used by multiple test

use anyhow::Context;
use std::ffi::{CStr, CString, c_char};
use std::fs;
use std::fs::create_dir;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null;

use crate::{TestSetup, krun_call};
use krun_sys::*;

fn copy_guest_agent(dir: &Path) -> anyhow::Result<()> {
    let path = std::env::var_os("KRUN_TEST_GUEST_AGENT_PATH")
        .context("KRUN_TEST_GUEST_AGENT_PATH env variable not set")?;

    let output_path = dir.join("guest-agent");
    fs::copy(path, output_path).context("Failed to copy executable into vm")?;
    Ok(())
}

/// Creates the root filesystem directory and copies the guest agent into it.
/// Returns the path to the root directory. Use this when you need to configure the root
/// filesystem yourself (e.g. via `krun_add_virtiofs3` for read-only mounts) rather than
/// using the default `setup_fs_and_enter`.
pub fn setup_rootfs(test_setup: &TestSetup) -> anyhow::Result<PathBuf> {
    let root_dir = test_setup.tmp_dir.join("rootfs");
    if !root_dir.exists() {
        create_dir(&root_dir).context("Failed to create rootfs directory")?;
    }
    copy_guest_agent(&root_dir)?;
    Ok(root_dir)
}

/// Sets up the root filesystem, copies the guest agent into it, and enters the VM.
pub fn setup_fs_and_enter(ctx: u32, test_setup: TestSetup) -> anyhow::Result<()> {
    setup_fs_and_enter_with_env(ctx, test_setup, &[])
}

pub fn setup_fs_and_enter_with_env(
    ctx: u32,
    test_setup: TestSetup,
    guest_env: &[&CStr],
) -> anyhow::Result<()> {
    let root_dir = setup_rootfs(&test_setup)?;

    let path_str = CString::new(root_dir.as_os_str().as_bytes()).context("CString::new")?;
    let mut envp: Vec<*const c_char> = guest_env
        .iter()
        .map(|entry| entry.as_ptr().cast())
        .collect();
    envp.push(null());
    unsafe {
        krun_call!(krun_set_root(ctx, path_str.as_ptr()))?;
        krun_call!(krun_set_workdir(ctx, c"/".as_ptr()))?;
        let test_case_cstr = CString::new(test_setup.test_case).context("CString::new")?;
        let argv = [test_case_cstr.as_ptr(), null()];
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

/// The format libkrun loads fxdax's guest kernel in: an Image on arm64, an ELF vmlinux on x86-64.
const GUEST_KERNEL_FORMAT: u32 = if cfg!(target_arch = "aarch64") {
    KRUN_KERNEL_FORMAT_RAW
} else {
    KRUN_KERNEL_FORMAT_ELF
};

/// Boots the kernel fxdax's scripts/build-guest-kernel.sh builds, which FXDAX_GUEST_KERNEL names,
/// instead of libkrunfw's, with libkrun's own command line.
pub fn set_guest_kernel(ctx: u32) -> anyhow::Result<()> {
    let kernel = CString::new(std::env::var("FXDAX_GUEST_KERNEL")?)?;
    unsafe {
        krun_call!(krun_set_kernel(
            ctx,
            kernel.as_ptr(),
            GUEST_KERNEL_FORMAT,
            null(),
            null()
        ))
    }
}

type KrunAddExtentStoreFn = unsafe extern "C" fn(
    ctx_id: u32,
    c_tag: *const c_char,
    c_path: *const c_char,
    capacity: u64,
    shm_size: u64,
) -> i32;

/// Adds an extent-store device tagged `tag`, on the store at `path`, which it makes, holding at
/// most `capacity` bytes, if it doesn't exist; with a DAX window of `shm_size` bytes if not 0.
pub fn add_extent_store(
    ctx: u32,
    tag: &CStr,
    path: &Path,
    capacity: u64,
    shm_size: u64,
) -> anyhow::Result<()> {
    let symbol = c"krun_add_extent_store";
    let ptr = unsafe { nix::libc::dlsym(nix::libc::RTLD_DEFAULT, symbol.as_ptr()) };
    anyhow::ensure!(!ptr.is_null(), "krun_add_extent_store not found");
    let krun_add_extent_store: KrunAddExtentStoreFn = unsafe { std::mem::transmute(ptr) };
    let path = CString::new(path.as_os_str().as_bytes())?;
    unsafe {
        krun_call!(krun_add_extent_store(
            ctx,
            tag.as_ptr(),
            path.as_ptr(),
            capacity,
            shm_size,
        ))
    }
}
