//! The VM an fxdax test boots: libkrun's, in this process, or, with FXDAX_VMM=firecracker,
//! fxdax's Firecracker, as FXDAX_FIRECRACKER names it, which this process runs until it exits.
//!
//! Under Firecracker, the root filesystem is an ext4 image of the root directory, on the first
//! virtio-blk disk, and the guest agent is the guest's init (`guest_init`), which gets its
//! environment and the test's name on the kernel's command line. The guest's console, its serial
//! port, goes to console.txt in the test's directory, and once the VM is done, to this process's
//! standard output, but for the guest agent's standard error, framed there, which goes to its
//! standard error, as under libkrun. Firecracker logs to its own file there too.

use anyhow::{Context, bail, ensure};
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null;

use crate::{krun_call, krun_call_u32};
use krun_sys::*;

/// The VMMs a test can boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vmm {
    Libkrun,
    Firecracker,
}

impl Vmm {
    /// The VMM FXDAX_VMM names: libkrun by default.
    pub fn from_env() -> Self {
        match std::env::var("FXDAX_VMM").as_deref() {
            Ok("firecracker") => Vmm::Firecracker,
            _ => Vmm::Libkrun,
        }
    }

    /// Whether the VMM has fxdax's extent-store device.
    pub fn has_extent_device(self) -> bool {
        match self {
            Vmm::Libkrun => {
                unsafe { krun_call_u32!(krun_has_feature(KRUN_FEATURE_FXDAX.into())) }.ok()
                    == Some(1)
            }
            Vmm::Firecracker => std::env::var_os("FXDAX_FIRECRACKER").is_some(),
        }
    }
}

/// A VM, as it is configured, until it boots.
pub struct Vm {
    vmm: VmmConfig,
    /// The guest's environment.
    env: Vec<CString>,
    /// The disks and pmem devices added.
    disks: usize,
    pmems: usize,
}

enum VmmConfig {
    Libkrun { ctx: u32 },
    Firecracker(serde_json::Value),
}

/// The format libkrun loads fxdax's guest kernel in: an Image on arm64, an ELF vmlinux on x86-64.
const GUEST_KERNEL_FORMAT: u32 = if cfg!(target_arch = "aarch64") {
    KRUN_KERNEL_FORMAT_RAW
} else {
    KRUN_KERNEL_FORMAT_ELF
};

/// The guest kernel's command line under Firecracker, before the guest's environment and the
/// test's name: the console on the serial port, saying nothing but emergencies; a reset on a
/// panic, which ends the VM; and the guest agent as init.
const FIRECRACKER_CMDLINE: &str = "console=ttyS0 loglevel=1 reboot=k panic=-1 8250.nr_uarts=1 i8042.noaux i8042.nomux \
     i8042.dumbkbd init=/guest-agent";

impl Vm {
    /// A VM of `cpus` vCPUs and `ram_mib` MiB of memory, on the VMM FXDAX_VMM names, which logs
    /// warnings only, whatever RUST_LOG says: logging every request costs time.
    pub fn new(cpus: u8, ram_mib: u32) -> anyhow::Result<Self> {
        let vmm = match Vmm::from_env() {
            Vmm::Libkrun => unsafe {
                krun_call!(krun_init_log(
                    KRUN_LOG_TARGET_DEFAULT,
                    KRUN_LOG_LEVEL_WARN,
                    KRUN_LOG_STYLE_NEVER,
                    KRUN_LOG_OPTION_NO_ENV,
                ))?;
                let ctx = krun_call_u32!(krun_create_ctx())?;
                krun_call!(krun_set_vm_config(ctx, cpus, ram_mib))?;
                VmmConfig::Libkrun { ctx }
            },
            Vmm::Firecracker => VmmConfig::Firecracker(serde_json::json!({
                "machine-config": { "vcpu_count": cpus, "mem_size_mib": ram_mib },
                "drives": [],
                "pmem": [],
                "extent": [],
            })),
        };
        Ok(Self {
            vmm,
            env: Vec::new(),
            disks: 0,
            pmems: 0,
        })
    }

    /// Boots the kernel fxdax's scripts/build-guest-kernel.sh builds, which FXDAX_GUEST_KERNEL
    /// names, instead of libkrunfw's.
    pub fn guest_kernel(&mut self) -> anyhow::Result<()> {
        let kernel = std::env::var("FXDAX_GUEST_KERNEL").context("FXDAX_GUEST_KERNEL not set")?;
        match &mut self.vmm {
            VmmConfig::Libkrun { ctx } => {
                let kernel = CString::new(kernel)?;
                unsafe {
                    krun_call!(krun_set_kernel(
                        *ctx,
                        kernel.as_ptr(),
                        GUEST_KERNEL_FORMAT,
                        null(),
                        null()
                    ))
                }
            }
            VmmConfig::Firecracker(config) => {
                config["boot-source"]["kernel_image_path"] = kernel.into();
                Ok(())
            }
        }
    }

    /// Adds an extent-store device tagged `tag`, on the store at `path`, which it makes, holding
    /// at most `capacity` bytes, if it doesn't exist; with a DAX window of `window` bytes if not
    /// 0. The extent daemon at FXDAX_EXTENTD serves the store.
    pub fn extent_store(
        &mut self,
        tag: &str,
        path: &Path,
        capacity: u64,
        window: u64,
    ) -> anyhow::Result<()> {
        let extentd = std::env::var_os("FXDAX_EXTENTD").context("FXDAX_EXTENTD not set")?;
        match &mut self.vmm {
            VmmConfig::Libkrun { ctx } => {
                type KrunAddExtentStoreFn = unsafe extern "C" fn(
                    ctx_id: u32,
                    c_tag: *const std::ffi::c_char,
                    c_path: *const std::ffi::c_char,
                    capacity: u64,
                    shm_size: u64,
                    c_extentd: *const std::ffi::c_char,
                ) -> i32;
                let symbol = c"krun_add_extent_store";
                let ptr = unsafe { nix::libc::dlsym(nix::libc::RTLD_DEFAULT, symbol.as_ptr()) };
                ensure!(!ptr.is_null(), "krun_add_extent_store not found");
                let krun_add_extent_store: KrunAddExtentStoreFn =
                    unsafe { std::mem::transmute(ptr) };
                let tag = CString::new(tag)?;
                let path = CString::new(path.as_os_str().as_bytes())?;
                let extentd = CString::new(extentd.as_bytes())?;
                unsafe {
                    krun_call!(krun_add_extent_store(
                        *ctx,
                        tag.as_ptr(),
                        path.as_ptr(),
                        capacity,
                        window,
                        extentd.as_ptr(),
                    ))
                }
            }
            VmmConfig::Firecracker(config) => {
                let mut device = serde_json::json!({
                    "id": tag,
                    "tag": tag,
                    "path_on_host": path,
                    "capacity": capacity,
                    "extentd_socket": PathBuf::from(extentd),
                });
                if window != 0 {
                    device["dax_window_size"] = window.into();
                }
                push(&mut config["extent"], device);
                Ok(())
            }
        }
    }

    /// Adds a writable virtio-pmem device on `image`, and returns its device in the guest.
    pub fn pmem(&mut self, id: &str, image: &Path) -> anyhow::Result<String> {
        let device = format!("/dev/pmem{}", self.pmems);
        self.pmems += 1;
        match &mut self.vmm {
            VmmConfig::Libkrun { ctx } => {
                let id = CString::new(id)?;
                let image = CString::new(image.as_os_str().as_bytes())?;
                unsafe { krun_call!(krun_add_pmem(*ctx, id.as_ptr(), image.as_ptr(), false))? };
            }
            VmmConfig::Firecracker(config) => push(
                &mut config["pmem"],
                serde_json::json!({ "id": id, "path_on_host": image }),
            ),
        }
        Ok(device)
    }

    /// Adds a writable virtio-blk disk on the raw `image`, which the host's page cache caches and
    /// whose flushes reach the host's disk, and returns its device in the guest.
    pub fn disk(&mut self, id: &str, image: &Path) -> anyhow::Result<String> {
        let index = self.disks;
        self.disks += 1;
        match &mut self.vmm {
            VmmConfig::Libkrun { ctx } => {
                let id = CString::new(id)?;
                let image = CString::new(image.as_os_str().as_bytes())?;
                unsafe {
                    krun_call!(krun_add_disk3(
                        *ctx,
                        id.as_ptr(),
                        image.as_ptr(),
                        KRUN_DISK_FORMAT_RAW,
                        false,
                        false,
                        KRUN_SYNC_RELAXED,
                    ))?;
                }
                // The root is a directory the host passes through, not a disk.
                Ok(format!("/dev/vd{}", disk_letter(index)))
            }
            VmmConfig::Firecracker(config) => {
                // The root's disk is the first.
                let device = format!("/dev/vd{}", disk_letter(index + 1));
                push(
                    &mut config["drives"],
                    serde_json::json!({
                        "drive_id": id,
                        "path_on_host": image,
                        "is_root_device": false,
                        "is_read_only": false,
                        "cache_type": "Writeback",
                    }),
                );
                Ok(device)
            }
        }
    }

    /// Adds libkrun's virtio-fs device tagged `tag`, passing the host directory `dir` through.
    pub fn virtiofs(&mut self, tag: &str, dir: &Path) -> anyhow::Result<()> {
        match &mut self.vmm {
            VmmConfig::Libkrun { ctx } => {
                let tag = CString::new(tag)?;
                let dir = CString::new(dir.as_os_str().as_bytes())?;
                unsafe { krun_call!(krun_add_virtiofs(*ctx, tag.as_ptr(), dir.as_ptr())) }
            }
            VmmConfig::Firecracker(_) => bail!("Firecracker has no virtio-fs device"),
        }
    }

    /// Sets `var`, `NAME=value`, in the guest agent's environment.
    pub fn env(&mut self, var: &CStr) {
        self.env.push(var.to_owned());
    }

    /// Boots the guest, with `root` as its root filesystem, running the guest agent there
    /// (`common::setup_rootfs` copies it) for `test_case`. It returns only if the VM cannot boot.
    pub fn enter(self, root: &Path, test_case: &str) -> anyhow::Result<()> {
        match self.vmm {
            VmmConfig::Libkrun { ctx } => {
                let root = CString::new(root.as_os_str().as_bytes())?;
                let test_case = CString::new(test_case)?;
                let argv = [test_case.as_ptr(), null()];
                let mut envp: Vec<_> = self.env.iter().map(|var| var.as_ptr()).collect();
                envp.push(null());
                unsafe {
                    krun_call!(krun_set_root(ctx, root.as_ptr()))?;
                    krun_call!(krun_set_workdir(ctx, c"/".as_ptr()))?;
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
            VmmConfig::Firecracker(config) => enter_firecracker(config, &self.env, root, test_case),
        }
    }
}

/// The letter of the virtio-blk disk at `index`: the guest names them vda, vdb, and so on.
fn disk_letter(index: usize) -> char {
    char::from(b'a' + u8::try_from(index).expect("few disks"))
}

fn push(array: &mut serde_json::Value, value: serde_json::Value) {
    array.as_array_mut().expect("an array").push(value);
}

/// Makes an ext4 image of `root`, and execs Firecracker with `config`, booting from it.
fn enter_firecracker(
    mut config: serde_json::Value,
    env: &[CString],
    root: &Path,
    test_case: &str,
) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let firecracker = std::env::var_os("FXDAX_FIRECRACKER").context("FXDAX_FIRECRACKER not set")?;
    ensure!(
        config["boot-source"]["kernel_image_path"].is_string(),
        "Firecracker boots fxdax's guest kernel only"
    );
    let dir = root.parent().context("the root has no parent")?;

    // What init mounts on.
    for mount_point in ["dev", "proc", "sys", "tmp"] {
        std::fs::create_dir_all(root.join(mount_point))?;
    }
    let image = dir.join("rootfs.ext4");
    // Room for the files, and some for the guest's own.
    let size = (tree_size(root)? * 5 / 4 + (256 << 20)).next_multiple_of(1 << 20);
    std::fs::File::create(&image)?.set_len(size)?;
    let status = std::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(root)
        .arg(&image)
        .status()
        .context("running mkfs.ext4")?;
    ensure!(
        status.success(),
        "mkfs.ext4 of {} failed: {status}",
        root.display()
    );

    let mut boot_args = FIRECRACKER_CMDLINE.to_owned();
    for var in env {
        let var = var
            .to_str()
            .context("the guest's environment is not UTF-8")?;
        ensure!(
            !var.contains(char::is_whitespace),
            "the kernel's command line cannot pass {var:?}"
        );
        boot_args.push(' ');
        boot_args.push_str(var);
    }
    boot_args.push_str(" -- ");
    boot_args.push_str(test_case);
    config["boot-source"]["boot_args"] = boot_args.into();
    config["drives"].as_array_mut().expect("an array").insert(
        0,
        serde_json::json!({
            "drive_id": "rootfs",
            "path_on_host": image,
            "is_root_device": true,
            "is_read_only": false,
        }),
    );

    let config_path = dir.join("firecracker.json");
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    let console_path = dir.join("console.txt");
    let mut command = std::process::Command::new(&firecracker);
    command
        .arg("--no-api")
        .arg("--config-file")
        .arg(&config_path)
        .arg("--log-path")
        .arg(dir.join("firecracker.log"))
        .arg("--level")
        .arg(std::env::var("FXDAX_FIRECRACKER_LOG_LEVEL").unwrap_or_else(|_| "Warning".into()))
        .stdin(std::process::Stdio::null())
        // A file, not a pipe: Firecracker drops what its serial port says when its standard
        // output would block.
        .stdout(std::fs::File::create(&console_path)?);
    // Firecracker goes when this process does, as when the runner times the test out.
    // SAFETY: prctl is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if nix::libc::prctl(nix::libc::PR_SET_PDEATHSIG, nix::libc::SIGKILL) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    };
    let mut child = command
        .spawn()
        .with_context(|| format!("executing {}", PathBuf::from(&firecracker).display()))?;
    let status = child.wait()?;
    split_console(std::fs::File::open(&console_path)?)?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Copies the guest's console, `console`, to standard output, but for the guest agent's standard
/// error, which `guest_init` frames there, to standard error, as libkrun separates them.
fn split_console(mut console: impl std::io::Read) -> anyhow::Result<()> {
    use std::io::Write;

    let (mut stdout, mut stderr) = (std::io::stdout().lock(), std::io::stderr().lock());
    let mut in_stderr = false;
    let mut buffer = vec![0; 4096];
    loop {
        let n = match console.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        };
        for run in
            buffer[..n].split_inclusive(|b| matches!(*b, crate::STDERR_START | crate::STDERR_END))
        {
            let (bytes, marker) = match run.split_last() {
                Some((&b, bytes)) if b == crate::STDERR_START || b == crate::STDERR_END => {
                    (bytes, Some(b))
                }
                _ => (run, None),
            };
            if in_stderr {
                stderr.write_all(bytes)?;
            } else {
                stdout.write_all(bytes)?;
            }
            match marker {
                Some(crate::STDERR_START) => in_stderr = true,
                Some(_) => in_stderr = false,
                None => {}
            }
        }
        stdout.flush()?;
    }
}

/// The bytes the files under `dir` hold.
fn tree_size(dir: &Path) -> anyhow::Result<u64> {
    let mut size = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        size += metadata.len();
        if metadata.is_dir() {
            size += tree_size(&entry.path())?;
        }
    }
    Ok(size)
}
