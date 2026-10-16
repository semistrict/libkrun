//! The guest agent as the guest's init, under a VMM without libkrun's own init (fxdax's
//! Firecracker): it sets the guest up as libkrun's init does before it runs the agent, and resets
//! the VM, which ends it, once the test is done.
//!
//! The guest's only console is the VMM's serial port, which carries both the agent's standard
//! output and its standard error: the error's bytes go between `STDERR_START` and `STDERR_END`,
//! which the host splits off (`vm`), so that the output stays the test's alone.

use nix::libc;
use std::ffi::CStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::time::Duration;

use crate::{STDERR_END, STDERR_START};

/// The guest, set up: what forwards the agent's standard error to the console.
pub struct Init {
    /// Says the forwarder has forwarded all there was.
    forwarded: mpsc::Receiver<()>,
}

/// Mounts what libkrun's init mounts, has the console pass the test's output as it is written,
/// and frames standard error on it.
pub fn set_up() -> Init {
    const NOEXEC: libc::c_ulong = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    let mounts: [(&CStr, &CStr, &CStr, libc::c_ulong); 6] = [
        // The kernel mounts it itself where it is configured to.
        (c"devtmpfs", c"/dev", c"devtmpfs", libc::MS_RELATIME),
        (c"proc", c"/proc", c"proc", NOEXEC | libc::MS_RELATIME),
        (c"sysfs", c"/sys", c"sysfs", NOEXEC | libc::MS_RELATIME),
        (
            c"cgroup2",
            c"/sys/fs/cgroup",
            c"cgroup2",
            NOEXEC | libc::MS_RELATIME,
        ),
        (
            c"devpts",
            c"/dev/pts",
            c"devpts",
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_RELATIME,
        ),
        (c"tmpfs", c"/dev/shm", c"tmpfs", NOEXEC | libc::MS_RELATIME),
    ];
    for (source, target, fstype, flags) in mounts {
        // SAFETY: Plain system calls on NUL-terminated strings.
        unsafe {
            libc::mkdir(target.as_ptr(), 0o755);
            if libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                flags,
                std::ptr::null(),
            ) < 0
                && std::io::Error::last_os_error().raw_os_error() != Some(libc::EBUSY)
            {
                eprintln!("mount({target:?}): {}", std::io::Error::last_os_error());
            }
        }
    }
    // The kernel gives init no PATH.
    if std::env::var_os("PATH").is_none() {
        // SAFETY: The agent has no other threads yet.
        unsafe {
            std::env::set_var(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
        };
    }
    // The console turns "\n" into "\r\n", but the host compares the output byte for byte.
    // SAFETY: termios is plain data, which tcgetattr fills.
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDOUT_FILENO, &mut termios) == 0 {
            termios.c_oflag &= !libc::OPOST;
            libc::tcsetattr(libc::STDOUT_FILENO, libc::TCSANOW, &termios);
        }
    }
    Init {
        forwarded: forward_stderr(),
    }
}

/// Makes standard error a pipe, which the agent's children inherit, and forwards what is written
/// to it to the console, a write at a time, each framed. Returns what says it is all forwarded.
fn forward_stderr() -> mpsc::Receiver<()> {
    let mut fds = [0; 2];
    // SAFETY: Plain system calls; the descriptors made are owned below.
    let (pipe, console) = unsafe {
        assert_eq!(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC), 0);
        assert_eq!(libc::dup2(fds[1], libc::STDERR_FILENO), libc::STDERR_FILENO);
        libc::close(fds[1]);
        let console = libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 0);
        assert!(console >= 0);
        (
            File::from(OwnedFd::from_raw_fd(fds[0])),
            File::from(OwnedFd::from_raw_fd(console)),
        )
    };
    let (done, forwarded) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut pipe, mut console) = (pipe, console);
        let mut buffer = vec![0; 4096];
        let mut frame = Vec::with_capacity(buffer.len() + 2);
        loop {
            let n = match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            frame.clear();
            frame.push(STDERR_START);
            frame.extend_from_slice(&buffer[..n]);
            frame.push(STDERR_END);
            // One write, which a write to standard output cannot come in the middle of.
            if console.write_all(&frame).is_err() {
                break;
            }
        }
        let _ = done.send(());
    });
    forwarded
}

impl Init {
    /// Resets the VM, which ends it, once the test's output has reached the host.
    pub fn reset(self) -> ! {
        let _ = std::io::stdout().flush();
        // The forwarder forwards all there is once nothing has standard error open, which a
        // process the test left running may still.
        // SAFETY: Closes the agent's standard error, which nothing uses from now on.
        unsafe { libc::close(libc::STDERR_FILENO) };
        let _ = self.forwarded.recv_timeout(Duration::from_secs(1));
        // SAFETY: Plain system calls.
        unsafe {
            // What the test wrote reaches the host's storage, as libkrun's init has it.
            libc::sync();
            libc::tcdrain(libc::STDOUT_FILENO);
            // Not a word more on the console: the kernel would say it restarts.
            const SYSLOG_ACTION_CONSOLE_OFF: libc::c_int = 6;
            libc::klogctl(SYSLOG_ACTION_CONSOLE_OFF, std::ptr::null_mut(), 0);
            libc::reboot(libc::RB_AUTOBOOT);
        }
        panic!("reboot: {}", std::io::Error::last_os_error());
    }
}
