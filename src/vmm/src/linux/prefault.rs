// KVM's mapping of guest memory for the guest ahead of its accesses
// (`KVM_PRE_FAULT_MEMORY`), for fxdax's DAX windows.
//
// KVM maps a page of guest memory into its stage 2 tables when the guest
// first touches it, an exit per page, which nested virtualization makes slow
// (each goes through the L1 host). fxdax maps a window's pages in batches, and
// has KVM map them for the guest as it does. `KVM_PRE_FAULT_MEMORY` is a vCPU
// ioctl, which waits for the vCPU to leave `KVM_RUN`, so a vCPU of its own
// does it, which the guest does not know of and which never runs. vCPUs share
// the stage 2 tables of the guest's memory, while none is in SMM or nested
// guest mode.

use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};

use devices::virtio::PrefaultGuestMemory;
use kvm_ioctls::VcpuFd;

use super::vstate::Vm;

// From `include/uapi/linux/kvm.h`.
const KVM_CAP_PRE_FAULT_MEMORY: libc::c_ulong = 236;
const KVM_PRE_FAULT_MEMORY: libc::c_ulong =
    (3 << 30) | ((size_of::<KvmPreFaultMemory>() as libc::c_ulong) << 16) | (0xae << 8) | 0xd5;

#[repr(C)]
struct KvmPreFaultMemory {
    gpa: u64,
    size: u64,
    flags: u64,
    padding: [u64; 5],
}

/// Maps guest memory for the guest with a vCPU of its own.
pub struct KvmPrefault {
    vcpu: VcpuFd,
    /// Whether a failure has been logged, which is logged only once.
    failed: AtomicBool,
}

impl KvmPrefault {
    /// A vCPU of `vm`'s, with id `id`, which no vCPU of the guest's has, for
    /// mapping guest memory ahead of the guest; or none if KVM cannot.
    pub fn new(vm: &Vm, id: u64) -> io::Result<Option<Self>> {
        if vm.fd().check_extension_raw(KVM_CAP_PRE_FAULT_MEMORY) <= 0 {
            return Ok(None);
        }
        let vcpu = vm.fd().create_vcpu(id).map_err(io::Error::other)?;
        // The stage 2 tables' depth follows the guest's physical address
        // width, which the vCPU's CPUID sets.
        vcpu.set_cpuid2(vm.supported_cpuid())
            .map_err(io::Error::other)?;
        Ok(Some(Self {
            vcpu,
            failed: AtomicBool::new(false),
        }))
    }
}

impl PrefaultGuestMemory for KvmPrefault {
    fn prefault(&self, guest_addr: u64, len: u64) {
        let mut range = KvmPreFaultMemory {
            gpa: guest_addr,
            size: len,
            flags: 0,
            padding: [0; 5],
        };
        // KVM maps what it can and updates the range to what is left, which
        // a signal, or contention, can leave.
        while range.size > 0 {
            // SAFETY: Passes the vCPU's ioctl the structure it takes, which
            // it updates.
            let ret = unsafe {
                libc::ioctl(
                    self.vcpu.as_raw_fd(),
                    KVM_PRE_FAULT_MEMORY as _,
                    &mut range as *mut KvmPreFaultMemory,
                )
            };
            if ret < 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR | libc::EAGAIN) => continue,
                    // The guest faults on what is left as it would have.
                    _ => {
                        if !self.failed.swap(true, Ordering::Relaxed) {
                            warn!(
                                "pre-faulting guest memory at {:#x}, {:#x} bytes: {err}",
                                range.gpa, range.size
                            );
                        }
                        return;
                    }
                }
            }
        }
    }
}
