//! The guest kernel's messages, which say what a filesystem logged when a guest test fails.

use nix::libc;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

/// Prints the kernel's messages, from /dev/kmsg, to standard error, each as "kernel: RECORD".
pub fn print() {
    let Ok(mut kmsg) = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/kmsg")
    else {
        return;
    };
    let mut record = [0u8; 8192];
    // Each read is a record, until there are no more.
    while let Ok(n) = kmsg.read(&mut record) {
        eprint!("kernel: {}", String::from_utf8_lossy(&record[..n]));
    }
}
