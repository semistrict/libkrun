use std::env;
use std::ffi::{CString, c_char};
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
const SNAPSHOT_DIR_NAME: &str = "snapshot";
const SNAPSHOT_METADATA: &str = "ubuntu-snapshot-v8-8gib-ram\n";
const RUNNER_PATH: &str = "/usr/local/bin/krun-command-runner";
const VSOCK_SOCKET: &str = "run/krun-command-runner.sock";
const EXIT_STATUS_MARKER: &str = "\0__KRUN_EXIT_STATUS__=";

#[link(name = "krun")]
unsafe extern "C" {
    fn krun_create_ctx() -> i32;
    fn krun_set_log_level(level: u32) -> i32;
    fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32;
    fn krun_set_root(ctx_id: u32, root_path: *const c_char) -> i32;
    fn krun_add_virtiofs3(
        ctx_id: u32,
        tag: *const c_char,
        path: *const c_char,
        shm_size: u64,
        read_only: bool,
    ) -> i32;
    fn krun_add_vsock_port2(
        ctx_id: u32,
        port: u32,
        filepath: *const c_char,
        listen: bool,
    ) -> i32;
    fn krun_set_exec(
        ctx_id: u32,
        exec_path: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> i32;
    fn krun_set_snapshot_path(ctx_id: u32, path: *const c_char) -> i32;
    fn krun_snapshot(ctx_id: u32, path: *const c_char) -> i32;
    fn krun_start_enter(ctx_id: u32) -> i32;
}

fn main() -> Result<()> {
    let command = parse_command_args()?;
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
    let rootfs = state_dir.join("rootfs").join(ROOTFS_NAME);
    let snapshot = state_dir.join(SNAPSHOT_DIR_NAME);
    let socket = rootfs.join(VSOCK_SOCKET);
    let have_snapshot = usable_snapshot(&snapshot);

    fs::create_dir_all(&state_dir).with_context(|| format!("create {}", state_dir.display()))?;
    ensure_rootfs(&rootfs)?;
    if !have_snapshot || !rootfs.join(RUNNER_PATH.trim_start_matches('/')).exists() {
        install_guest_runner(&rootfs)?;
    }
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let _ = fs::remove_file(&socket);

    let (run_tx, run_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    spawn_command_client(socket.clone(), command, run_rx, done_tx);

    unsafe {
        call_i32(krun_set_log_level(2), "krun_set_log_level")?;
        let ctx = krun_create_ctx();
        if ctx < 0 {
            bail_krun(ctx, "krun_create_ctx")?;
        }
        let ctx = ctx as u32;

        call_i32(krun_set_vm_config(ctx, 2, 8192), "krun_set_vm_config")?;

        let rootfs_c = cstring_path(&rootfs)?;
        call_i32(krun_set_root(ctx, rootfs_c.as_ptr()), "krun_set_root")?;

        let home_tag_c = CString::new(HOME_TAG)?;
        let home_c = cstring_path(&home)?;
        call_i32(
            krun_add_virtiofs3(ctx, home_tag_c.as_ptr(), home_c.as_ptr(), 0, false),
            "krun_add_virtiofs3(home)",
        )?;

        let socket_c = cstring_path(&socket)?;
        call_i32(
            krun_add_vsock_port2(ctx, RUNNER_PORT, socket_c.as_ptr(), true),
            "krun_add_vsock_port2(runner)",
        )?;

        if have_snapshot {
            let snapshot_c = cstring_path(&snapshot)?;
            call_i32(
                krun_set_snapshot_path(ctx, snapshot_c.as_ptr()),
                "krun_set_snapshot_path",
            )?;
        }

        configure_runner(ctx, &home, &cwd)?;
        spawn_snapshot_after_command(ctx, snapshot.clone(), done_rx);
        run_tx.send(()).context("start command client")?;

        let rc = krun_start_enter(ctx);
        bail_krun(rc, "krun_start_enter")?;
    }

    Ok(())
}

fn parse_command_args() -> Result<Vec<String>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        bail!("usage: ubuntu_snapshot <command> [args...]");
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

fn install_guest_runner(rootfs: &Path) -> Result<()> {
    let runner_path = rootfs.join(RUNNER_PATH.trim_start_matches('/'));
    let source_path = rootfs.join("usr/local/src/krun-command-runner.c");
    if let Some(parent) = runner_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if let Some(parent) = source_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    fs::write(&source_path, guest_runner_source())
        .with_context(|| format!("write {}", source_path.display()))?;
    run(Command::new("zig")
        .args([
            "cc",
            "-target",
            "aarch64-linux-musl",
            "-static",
            "-O2",
        ])
        .arg(&source_path)
        .arg("-o")
        .arg(&runner_path))?;
    Ok(())
}

fn guest_runner_source() -> &'static str {
    r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/vm_sockets.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static void die(const char *msg) {
    perror(msg);
    _exit(125);
}

static void mkdir_p(const char *path) {
    char tmp[4096];
    snprintf(tmp, sizeof(tmp), "%s", path);
    for (char *p = tmp + 1; *p; p++) {
        if (*p == '/') {
            *p = 0;
            mkdir(tmp, 0755);
            *p = '/';
        }
    }
    mkdir(tmp, 0755);
}

static int listen_vsock(uint32_t port) {
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) die("socket(AF_VSOCK)");

    struct sockaddr_vm addr;
    memset(&addr, 0, sizeof(addr));
    addr.svm_family = AF_VSOCK;
    addr.svm_cid = VMADDR_CID_ANY;
    addr.svm_port = port;
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) die("bind(vsock)");
    if (listen(fd, 32) < 0) die("listen(vsock)");
    return fd;
}

static void write_all(int fd, const void *buf, size_t len) {
    const char *p = buf;
    while (len) {
        ssize_t n = write(fd, p, len);
        if (n < 0) {
            if (errno == EINTR) continue;
            return;
        }
        p += n;
        len -= (size_t)n;
    }
}

static void read_exact(int fd, void *buf, size_t len) {
    char *p = buf;
    while (len) {
        ssize_t n = read(fd, p, len);
        if (n < 0) {
            if (errno == EINTR) continue;
            die("read");
        }
        if (n == 0) die("short read");
        p += n;
        len -= (size_t)n;
    }
}

static uint32_t read_u32(int fd) {
    uint8_t buf[4];
    read_exact(fd, buf, sizeof(buf));
    return ((uint32_t)buf[0] << 24) | ((uint32_t)buf[1] << 16) | ((uint32_t)buf[2] << 8) | (uint32_t)buf[3];
}

static char **read_argv(int fd) {
    uint32_t argc = read_u32(fd);
    if (argc == 0 || argc > 4096) die("bad argc");
    char **argv = calloc((size_t)argc + 1, sizeof(char *));
    if (!argv) die("calloc");
    for (uint32_t i = 0; i < argc; i++) {
        uint32_t len = read_u32(fd);
        if (len > 1024 * 1024) die("bad argv len");
        argv[i] = malloc((size_t)len + 1);
        if (!argv[i]) die("malloc argv");
        read_exact(fd, argv[i], len);
        argv[i][len] = 0;
    }
    return argv;
}

static void free_argv(char **argv) {
    if (!argv) return;
    for (char **p = argv; *p; p++) free(*p);
    free(argv);
}

static int run_command(char **argv, int out_fd) {
    int pipefd[2];
    if (pipe2(pipefd, O_CLOEXEC) < 0) die("pipe2");
    pid_t pid = fork();
    if (pid < 0) die("fork");
    if (pid == 0) {
        close(pipefd[0]);
        dup2(pipefd[1], STDOUT_FILENO);
        dup2(pipefd[1], STDERR_FILENO);
        close(pipefd[1]);
        execvp(argv[0], argv);
        _exit(127);
    }
    close(pipefd[1]);
    int flags = fcntl(pipefd[0], F_GETFL);
    if (flags >= 0) fcntl(pipefd[0], F_SETFL, flags | O_NONBLOCK);

    char buf[8192];
    int status = 0;
    int child_exited = 0;
    for (;;) {
        ssize_t n = read(pipefd[0], buf, sizeof(buf));
        if (n < 0) {
            if (errno == EINTR) continue;
            if (errno != EAGAIN && errno != EWOULDBLOCK) break;
        } else if (n == 0) {
            break;
        } else {
            write_all(out_fd, buf, (size_t)n);
        }

        if (!child_exited) {
            pid_t waited = waitpid(pid, &status, WNOHANG);
            if (waited == pid) child_exited = 1;
        }
        if (child_exited) {
            for (;;) {
                n = read(pipefd[0], buf, sizeof(buf));
                if (n > 0) {
                    write_all(out_fd, buf, (size_t)n);
                    continue;
                }
                if (n < 0 && errno == EINTR) continue;
                break;
            }
            break;
        }
        usleep(10000);
    }
    close(pipefd[0]);
    if (!child_exited) waitpid(pid, &status, 0);
    if (WIFEXITED(status)) return WEXITSTATUS(status);
    if (WIFSIGNALED(status)) return 128 + WTERMSIG(status);
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 5) return 125;
    const char *home_tag = argv[1];
    const char *host_home = argv[2];
    const char *workdir = argv[3];
    uint32_t port = (uint32_t)strtoul(argv[4], NULL, 10);

    signal(SIGPIPE, SIG_IGN);
    mkdir_p(host_home);
    if (mount(home_tag, host_home, "virtiofs", 0, "") < 0 && errno != EBUSY) die("mount home");
    if (chdir(workdir) < 0) die("chdir");

    int listener = listen_vsock(port);
    for (;;) {
        int fd = accept(listener, NULL, NULL);
        if (fd < 0) {
            if (errno == EINTR) continue;
            die("accept(vsock)");
        }
        char **argv = read_argv(fd);
        int status = run_command(argv, fd);
        free_argv(argv);
        char trailer[64];
        trailer[0] = 0;
        int n = snprintf(trailer + 1, sizeof(trailer) - 1, "__KRUN_EXIT_STATUS__=%d\n", status);
        write_all(fd, trailer, (size_t)n + 1);
        close(fd);
    }
}
"#
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
        unsafe { krun_set_exec(ctx, exec_path.as_ptr(), argv.as_ptr(), envp.as_ptr()) },
        "krun_set_exec",
    )
}

fn spawn_command_client(
    socket: PathBuf,
    command: Vec<String>,
    start: mpsc::Receiver<()>,
    done: mpsc::Sender<Result<i32, String>>,
) {
    thread::spawn(move || {
        let result = (|| -> Result<i32> {
            start.recv().context("wait for VM start")?;
            let mut stream = connect_unix(&socket, Duration::from_secs(30))
                .with_context(|| format!("connect {}", socket.display()))?;
            write_command_vector(&mut stream, &command).context("send command")?;

            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).context("read command output")?;

            let marker = EXIT_STATUS_MARKER.as_bytes();
            let marker_pos = buf
                .windows(marker.len())
                .rposition(|window| window == marker)
                .with_context(|| {
                    let preview = String::from_utf8_lossy(&buf[..buf.len().min(400)]);
                    format!("runner closed without exit trailer; output preview: {preview:?}")
                })?;
            std::io::stdout()
                .write_all(&buf[..marker_pos])
                .context("write command output")?;
            std::io::stdout().flush().context("flush command output")?;

            let status_text = std::str::from_utf8(&buf[marker_pos + marker.len()..])
                .context("runner exit status was not utf-8")?
                .trim_end();
            let exit_status = status_text
                .parse()
                .with_context(|| format!("invalid runner exit status {status_text:?}"))?;
            Ok(exit_status)
        })();
        let _ = done.send(result.map_err(|e| format!("{e:#}")));
    });
}

fn write_command_vector(stream: &mut UnixStream, command: &[String]) -> Result<()> {
    write_u32(stream, command.len().try_into().context("too many command arguments")?)?;
    for arg in command {
        let bytes = arg.as_bytes();
        write_u32(stream, bytes.len().try_into().context("command argument too large")?)?;
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
    done: mpsc::Receiver<Result<i32, String>>,
) {
    thread::spawn(move || {
        let exit_status = match done.recv() {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("command client failed: {e}");
                std::process::exit(1);
            }
        };
        thread::sleep(Duration::from_millis(250));
        let snapshot_c = cstring_path(&snapshot).unwrap_or_else(|e| {
            eprintln!("{e:#}");
            std::process::exit(1);
        });
        let rc = unsafe { krun_snapshot(ctx, snapshot_c.as_ptr()) };
        if rc != 0 {
            eprintln!("krun_snapshot failed: {}", os_error(rc));
            std::process::exit(1);
        }
        if let Err(e) = fs::write(snapshot.join("metadata"), SNAPSHOT_METADATA) {
            eprintln!("write snapshot metadata failed: {e:#}");
            std::process::exit(1);
        }
        std::process::exit(exit_status);
    });
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
