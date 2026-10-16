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
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::time::Duration;

use crate::{STDERR_END, STDERR_START};

/// The guest, set up: the console, and what forwards the agent's output to it.
pub struct Init {
    console: File,
    /// Say the forwarders have forwarded all there was.
    forwarded: Vec<mpsc::Receiver<()>>,
}

/// Mounts what libkrun's init mounts, has the console pass the test's output as it is written,
/// and forwards the agent's output there, framing standard error.
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
    // The workload's output goes through pipes, as libkrun's init has it, so that programs that
    // act differently on a terminal, such as fio, do not.
    // SAFETY: Duplicates the console, which the copy owned below refers to.
    let console = unsafe {
        let console = libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 0);
        assert!(console >= 0, "dup: {}", std::io::Error::last_os_error());
        File::from(OwnedFd::from_raw_fd(console))
    };
    let forwarded = vec![
        forward(libc::STDOUT_FILENO, None, &console),
        forward(
            libc::STDERR_FILENO,
            Some((STDERR_START, STDERR_END)),
            &console,
        ),
    ];
    Init { console, forwarded }
}

/// Makes `fd` a pipe, which the agent's children inherit, and forwards what is written to it to
/// the console, a write at a time, each between `frame`'s bytes if any. Returns what says it is
/// all forwarded.
fn forward(fd: libc::c_int, frame: Option<(u8, u8)>, console: &File) -> mpsc::Receiver<()> {
    let mut fds = [0; 2];
    // SAFETY: Plain system calls; the read end is owned below.
    let pipe = unsafe {
        assert_eq!(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC), 0);
        assert_eq!(libc::dup2(fds[1], fd), fd);
        libc::close(fds[1]);
        File::from(OwnedFd::from_raw_fd(fds[0]))
    };
    let mut console = console.try_clone().expect("dup the console");
    let (done, forwarded) = mpsc::channel();
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut buffer = vec![0; 4096];
        let mut framed = Vec::with_capacity(buffer.len() + 2);
        loop {
            let n = match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            framed.clear();
            match frame {
                Some((start, end)) => {
                    framed.push(start);
                    framed.extend_from_slice(&buffer[..n]);
                    framed.push(end);
                }
                None => framed.extend_from_slice(&buffer[..n]),
            }
            // One write, which the other forwarder's cannot come in the middle of.
            if console.write_all(&framed).is_err() {
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
        // The forwarders forward all there is once nothing has the pipes open, which a process
        // the test left running may still.
        // SAFETY: Closes the agent's output, which nothing uses from now on.
        unsafe {
            libc::close(libc::STDOUT_FILENO);
            libc::close(libc::STDERR_FILENO);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        for forwarded in &self.forwarded {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let _ = forwarded.recv_timeout(left);
        }
        // SAFETY: Plain system calls.
        unsafe {
            // What the test wrote reaches the host's storage, as libkrun's init has it.
            libc::sync();
            libc::tcdrain(self.console.as_raw_fd());
            // Not a word more on the console: the kernel would say it restarts.
            const SYSLOG_ACTION_CONSOLE_OFF: libc::c_int = 6;
            libc::klogctl(SYSLOG_ACTION_CONSOLE_OFF, std::ptr::null_mut(), 0);
            libc::reboot(libc::RB_AUTOBOOT);
        }
        panic!("reboot: {}", std::io::Error::last_os_error());
    }
}
