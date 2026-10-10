// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter, Result};

// On x86-64 KVM the guest polls for a while before halting (cpuidle-haltpoll), rather than KVM
// polling for it: a request to a device whose reply comes within the poll costs the vCPU no halt,
// and no wakeup, which nested virtualization makes slow.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 \
                                          rootfstype=virtiofs rw quiet no-kvmapf \
                                          cpuidle_haltpoll.force=1";
#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
pub const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 \
                                          rootfstype=virtiofs rw quiet no-kvmapf";
#[cfg(target_os = "macos")]
pub const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 \
                                          rootfstype=virtiofs rw quiet no-kvmapf";

/// Strongly typed data structure used to configure the boot source of the
/// microvm.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct KernelCmdlineConfig {
    pub prolog: Option<String>,
    pub krun_env: Option<String>,
    pub epilog: Option<String>,
}

/// Errors associated with actions on `KernelCmdlineConfig`.
#[derive(Debug)]
pub enum KernelCmdlineConfigError {
    /// The kernel command line is invalid.
    InvalidKernelCommandLine(String),
}

impl Display for KernelCmdlineConfigError {
    fn fmt(&self, f: &mut Formatter) -> Result {
        use self::KernelCmdlineConfigError::*;
        match *self {
            InvalidKernelCommandLine(ref e) => {
                write!(f, "The kernel command line is invalid: {}", e.as_str())
            }
        }
    }
}
