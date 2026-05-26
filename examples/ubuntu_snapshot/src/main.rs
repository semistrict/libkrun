use std::env;
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const UBUNTU_IMAGE: &str = "ubuntu:26.04";
const ROOTFS_NAME: &str = "ubuntu-26.04";
const HOME_TAG: &str = "krun-home";
const RUNNER_PORT: u32 = 10240;
const SNAPSHOT_DIR_NAME: &str = "krun-snapshot-v17";
const SNAPSHOT_METADATA: &str = "ubuntu-snapshot-v17-detached-console-output\n";
const RUNNER_PATH: &str = "/usr/local/bin/krun-command-runner";
const VSOCK_SOCKET: &str = "run/krun-command-runner.sock";
const EXIT_STATUS_MARKER: &str = "\0__KRUN_EXIT_STATUS__=";
const DISABLE_SNAPSHOT_RESTORE_ENV: &str = "KRUN_DISABLE_SNAPSHOT_RESTORE";
const ENOENT: i32 = 2;

fn main() -> Result<()> {
    let command = parse_command_args()?;
    let terminal_state = terminal_state();
    let home = dirs::home_dir().context("could not resolve home directory")?;
    let cwd = env::current_dir().context("current directory")?;
    if !cwd.starts_with(&home) {
        bail!(
            "current directory {} is not inside home {}; this example mounts only home",
            cwd.display(),
            home.display()
        );
    }

    let state_dir = home.join(".libkrun");
    let run_dir = state_dir.join("run");
    let rootfs = state_dir.join("rootfs").join(ROOTFS_NAME);
    let snapshot = state_dir.join(SNAPSHOT_DIR_NAME);
    let socket = rootfs.join(VSOCK_SOCKET);
    let console_output = run_dir.join("krun.console.log");
    let restore_disabled = env::var_os(DISABLE_SNAPSHOT_RESTORE_ENV).is_some();
    let have_snapshot = !restore_disabled && usable_snapshot(&snapshot);
    if !have_snapshot && snapshot.exists() {
        fs::remove_dir_all(&snapshot)
            .with_context(|| format!("remove incompatible snapshot {}", snapshot.display()))?;
    }

    fs::create_dir_all(&state_dir).with_context(|| format!("create {}", state_dir.display()))?;
    fs::create_dir_all(&run_dir).with_context(|| format!("create {}", run_dir.display()))?;
    install_firmware(&state_dir)?;
    ensure_rootfs(&rootfs)?;
    install_guest_runner(&rootfs)?;
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let _ = fs::remove_file(&socket);

    let (run_tx, run_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();

    unsafe {
        call_i32(krun::krun_set_log_level(2), "krun::krun_set_log_level")?;
        let ctx = krun::krun_create_ctx();
        if ctx < 0 {
            bail_krun(ctx, "krun::krun_create_ctx")?;
        }
        let ctx = ctx as u32;

        let console_output_c = cstring_path(&console_output)?;
        call_i32(
            krun::krun_set_console_output(ctx, console_output_c.as_ptr()),
            "krun::krun_set_console_output",
        )?;

        call_i32(
            krun::krun_set_vm_config(ctx, 2, 8192),
            "krun::krun_set_vm_config",
        )?;

        let rootfs_c = cstring_path(&rootfs)?;
        call_i32(
            krun::krun_set_root(ctx, rootfs_c.as_ptr()),
            "krun::krun_set_root",
        )?;

        let home_tag_c = CString::new(HOME_TAG)?;
        let home_c = cstring_path(&home)?;
        call_i32(
            krun::krun_add_virtiofs3(ctx, home_tag_c.as_ptr(), home_c.as_ptr(), 0, false),
            "krun::krun_add_virtiofs3(home)",
        )?;

        let socket_c = cstring_path(&socket)?;
        call_i32(
            krun::krun_add_vsock_port2(ctx, RUNNER_PORT, socket_c.as_ptr(), true),
            "krun::krun_add_vsock_port2(runner)",
        )?;

        if have_snapshot {
            let snapshot_c = cstring_path(&snapshot)?;
            call_i32(
                krun::krun_set_snapshot_path(ctx, snapshot_c.as_ptr()),
                "krun::krun_set_snapshot_path",
            )?;
        }

        configure_runner(ctx, &home, &cwd)?;
        spawn_command_client(ctx, have_snapshot, socket.clone(), command, run_rx, done_tx);
        spawn_snapshot_after_command(ctx, snapshot.clone(), terminal_state.clone(), done_rx);
        run_tx.send(()).context("start command client")?;

        let rc = krun::krun_start_enter(ctx);
        if rc != 0 && have_snapshot {
            restart_without_snapshot(&snapshot, &terminal_state)?;
        }
        bail_krun(rc, "krun::krun_start_enter")?;
    }

    Ok(())
}

fn restart_without_snapshot(snapshot: &Path, terminal_state: &Option<String>) -> Result<()> {
    let _ = fs::remove_dir_all(snapshot);
    restore_terminal(terminal_state);
    let status = Command::new(env::current_exe().context("current executable")?)
        .args(env::args_os().skip(1))
        .env(DISABLE_SNAPSHOT_RESTORE_ENV, "1")
        .status()
        .context("restart without snapshot restore")?;
    std::process::exit(status.code().unwrap_or(1));
}

fn parse_command_args() -> Result<Vec<String>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        bail!("usage: krun <command> [args...]");
    }
    if args[0].is_empty() {
        bail!("command must not be empty");
    }
    Ok(args)
}

fn ensure_rootfs(rootfs: &Path) -> Result<()> {
    if !rootfs.exists() {
        fs::create_dir_all(rootfs).with_context(|| format!("create {}", rootfs.display()))?;
    }
    if !rootfs.join("bin/sh").exists() {
        fs::remove_dir_all(rootfs)
            .with_context(|| format!("remove incomplete rootfs {}", rootfs.display()))?;
        fs::create_dir_all(rootfs).with_context(|| format!("create {}", rootfs.display()))?;

        let status = Command::new("docker")
            .args(["image", "inspect", UBUNTU_IMAGE])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("docker image inspect")?;
        if !status.success() {
            run(Command::new("docker").args(["pull", UBUNTU_IMAGE]))?;
        }

        let output = Command::new("docker")
            .args(["create", UBUNTU_IMAGE])
            .output()
            .context("docker create")?;
        if !output.status.success() {
            bail!(
                "docker create failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let container_id = String::from_utf8(output.stdout)
            .context("docker create output")?
            .trim()
            .to_string();

        let export = Command::new("docker")
            .args(["export", &container_id])
            .stdout(Stdio::piped())
            .spawn()
            .context("docker export")?;
        let export_stdout = export.stdout.context("docker export stdout")?;
        let mut tar = Command::new("tar")
            .args(["-xpf", "-", "-C"])
            .arg(rootfs)
            .stdin(Stdio::from(export_stdout))
            .spawn()
            .context("tar extract rootfs")?;
        let tar_status = tar.wait().context("wait for tar")?;
        let _ = Command::new("docker")
            .args(["rm", &container_id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !tar_status.success() {
            bail!("tar extraction failed with {tar_status}");
        }
    }
    Ok(())
}

fn usable_snapshot(snapshot: &Path) -> bool {
    snapshot.join("vmstate.bin").exists()
        && snapshot.join("pages.img").exists()
        && fs::read_to_string(snapshot.join("metadata"))
            .map(|metadata| metadata == SNAPSHOT_METADATA)
            .unwrap_or(false)
}

fn install_firmware(state_dir: &Path) -> Result<()> {
    let lib_dir = state_dir.join("lib");
    fs::create_dir_all(&lib_dir).with_context(|| format!("create {}", lib_dir.display()))?;
    let firmware = lib_dir.join("libkrunfw.5.dylib");
    fs::write(
        &firmware,
        include_bytes!(concat!(env!("OUT_DIR"), "/libkrunfw.5.dylib")),
    )
    .with_context(|| format!("write {}", firmware.display()))?;
    // SAFETY: This process controls its own environment before libkrun lazily
    // dlopens libkrunfw.
    unsafe {
        env::set_var("KRUNFW_PATH", &firmware);
    }
    Ok(())
}

fn install_guest_runner(rootfs: &Path) -> Result<()> {
    let runner_path = rootfs.join(RUNNER_PATH.trim_start_matches('/'));
    if let Some(parent) = runner_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    fs::write(
        &runner_path,
        include_bytes!(concat!(env!("OUT_DIR"), "/krun-command-runner")),
    )
    .with_context(|| format!("write {}", runner_path.display()))?;
    Ok(())
}

unsafe fn configure_runner(ctx: u32, home: &Path, cwd: &Path) -> Result<()> {
    let exec_path = CString::new(RUNNER_PATH)?;
    let home_tag_c = CString::new(HOME_TAG)?;
    let home_c = cstring_path(home)?;
    let cwd_c = cstring_path(cwd)?;
    let port_c = CString::new(RUNNER_PORT.to_string())?;
    let argv = [
        home_tag_c.as_ptr(),
        home_c.as_ptr(),
        cwd_c.as_ptr(),
        port_c.as_ptr(),
        ptr::null(),
    ];
    let envp = [
        c"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".as_ptr(),
        ptr::null(),
    ];
    call_i32(
        unsafe { krun::krun_set_exec(ctx, exec_path.as_ptr(), argv.as_ptr(), envp.as_ptr()) },
        "krun::krun_set_exec",
    )
}

fn spawn_command_client(
    ctx: u32,
    have_snapshot: bool,
    socket: PathBuf,
    command: Vec<String>,
    start: mpsc::Receiver<()>,
    done: mpsc::Sender<Result<CommandResult, String>>,
) {
    thread::spawn(move || {
        let result = (|| -> Result<CommandResult> {
            start.recv().context("wait for VM start")?;
            if have_snapshot {
                arm_dirty_tracking(ctx)?;
            }
            let mut stream = connect_unix(&socket, Duration::from_secs(30))
                .with_context(|| format!("connect {}", socket.display()))?;
            write_command_vector(&mut stream, &command).context("send command")?;

            let mut buf = Vec::new();
            stream
                .read_to_end(&mut buf)
                .context("read command output")?;

            let marker = EXIT_STATUS_MARKER.as_bytes();
            let marker_pos = buf
                .windows(marker.len())
                .rposition(|window| window == marker)
                .with_context(|| {
                    let preview = String::from_utf8_lossy(&buf[..buf.len().min(400)]);
                    format!("runner closed without exit trailer; output preview: {preview:?}")
                })?;
            let status_text = std::str::from_utf8(&buf[marker_pos + marker.len()..])
                .context("runner exit status was not utf-8")?
                .trim_end();
            let exit_status = status_text
                .parse()
                .with_context(|| format!("invalid runner exit status {status_text:?}"))?;
            Ok(CommandResult {
                exit_status,
                output: buf[..marker_pos].to_vec(),
            })
        })();
        let _ = done.send(result.map_err(|e| format!("{e:#}")));
    });
}

struct CommandResult {
    exit_status: i32,
    output: Vec<u8>,
}

fn arm_dirty_tracking(ctx: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let rc = krun::krun_arm_dirty_tracking(ctx);
        if rc == 0 {
            return Ok(());
        }
        if rc != -ENOENT || Instant::now() >= deadline {
            bail_krun(rc, "krun::krun_arm_dirty_tracking")?;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn write_command_vector(stream: &mut UnixStream, command: &[String]) -> Result<()> {
    write_u32(
        stream,
        command
            .len()
            .try_into()
            .context("too many command arguments")?,
    )?;
    for arg in command {
        let bytes = arg.as_bytes();
        write_u32(
            stream,
            bytes
                .len()
                .try_into()
                .context("command argument too large")?,
        )?;
        stream.write_all(bytes)?;
    }
    Ok(())
}

fn write_u32(stream: &mut UnixStream, value: u32) -> Result<()> {
    stream.write_all(&value.to_be_bytes())?;
    Ok(())
}

fn connect_unix(path: &Path, timeout: Duration) -> Result<UnixStream> {
    let start = Instant::now();
    let mut last_error = None;
    while start.elapsed() < timeout {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                last_error = Some(e);
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
    match last_error {
        Some(e) => Err(e).with_context(|| format!("timed out connecting to {}", path.display())),
        None => bail!("timed out connecting to {}", path.display()),
    }
}

fn spawn_snapshot_after_command(
    ctx: u32,
    snapshot: PathBuf,
    terminal_state: Option<String>,
    done: mpsc::Receiver<Result<CommandResult, String>>,
) {
    thread::spawn(move || {
        let command_result = match done.recv() {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                restore_terminal(&terminal_state);
                eprintln!("{e}");
                std::process::exit(1);
            }
            Err(e) => {
                restore_terminal(&terminal_state);
                eprintln!("command client failed: {e}");
                std::process::exit(1);
            }
        };
        thread::sleep(Duration::from_millis(250));
        let snapshot_c = cstring_path(&snapshot).unwrap_or_else(|e| {
            restore_terminal(&terminal_state);
            eprintln!("{e:#}");
            std::process::exit(1);
        });
        let started = Instant::now();
        let rc = unsafe { krun::krun_snapshot(ctx, snapshot_c.as_ptr()) };
        let snapshot_ms = started.elapsed().as_millis();
        if rc != 0 {
            restore_terminal(&terminal_state);
            eprintln!("krun::krun_snapshot failed: {}", os_error(rc));
            std::process::exit(1);
        }
        if let Err(e) = fs::write(snapshot.join("metadata"), SNAPSHOT_METADATA) {
            restore_terminal(&terminal_state);
            eprintln!("write snapshot metadata failed: {e:#}");
            std::process::exit(1);
        }
        if let Some(home) = dirs::home_dir() {
            let _ = fs::write(
                home.join(".libkrun/run/ubuntu_snapshot.snapshot_ms"),
                format!("{snapshot_ms}\n"),
            );
        }
        restore_terminal(&terminal_state);
        write_command_output_or_exit(&command_result.output);
        std::process::exit(command_result.exit_status);
    });
}

fn write_command_output_or_exit(output: &[u8]) {
    if let Err(e) = std::io::stdout().write_all(output) {
        eprintln!("write command output failed: {e}");
        std::process::exit(1);
    }
    if let Err(e) = std::io::stdout().flush() {
        eprintln!("flush command output failed: {e}");
        std::process::exit(1);
    }
}

fn terminal_state() -> Option<String> {
    let output = Command::new("stty")
        .args(["-f", "/dev/tty", "-g"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn restore_terminal(state: &Option<String>) {
    if let Some(state) = state {
        let _ = Command::new("stty")
            .args(["-f", "/dev/tty", state])
            .status();
    }
}

fn cstring_path(path: &Path) -> Result<CString> {
    CString::new(path.to_string_lossy().as_bytes())
        .with_context(|| format!("path contains NUL: {}", path.display()))
}

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("run {:?}", command))?;
    if !status.success() {
        bail!("{:?} failed with {status}", command);
    }
    Ok(())
}

fn call_i32(rc: i32, name: &str) -> Result<()> {
    if rc < 0 {
        bail_krun(rc, name)?;
    }
    Ok(())
}

fn bail_krun<T>(rc: i32, name: &str) -> Result<T> {
    bail!("{name} failed: {}", os_error(rc))
}

fn os_error(rc: i32) -> String {
    if rc < 0 {
        std::io::Error::from_raw_os_error(-rc).to_string()
    } else {
        format!("unexpected return code {rc}")
    }
}
