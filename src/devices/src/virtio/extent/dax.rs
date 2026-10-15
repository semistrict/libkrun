// The extent device's DAX window: the device's shared memory region, whose
// host memory KVM follows. fxdax's extent daemon owns the window: the device
// hands the region's host memory to the store's connection, which maps the
// window's pages there from the daemon's memory, and passes it the faults KVM
// takes on the rest. KVM would map each page for the guest on a fault of the
// guest's, which nested virtualization makes slow: given a
// `PrefaultGuestMemory`, the window has KVM map the pages as it maps them.

use std::io;
use std::sync::Arc;

use fxdax::RemoteExtentStore;

use crate::virtio::VirtioShmRegion;

/// Maps guest memory for the guest ahead of its accesses, as KVM's
/// `KVM_PRE_FAULT_MEMORY` does, rather than on a fault on each page.
pub trait PrefaultGuestMemory: Send + Sync {
    /// Maps the `len` bytes of guest memory at `guest_addr`, which the host
    /// has mapped, for the guest. It must not wait for the guest.
    fn prefault(&self, guest_addr: u64, len: u64);
}

/// Has `prefault` map the pages the window maps, at its guest address.
struct WindowPrefault {
    guest_addr: u64,
    prefault: Arc<dyn PrefaultGuestMemory>,
}

impl fxdax::Prefault for WindowPrefault {
    fn prefault(&self, offset: u64, len: u64) {
        self.prefault.prefault(self.guest_addr + offset, len);
    }
}

/// Creates `store`'s DAX window in the host memory of `shm_region`. With
/// `prefault`, KVM maps the pages for the guest as the window maps them.
pub(super) fn create_dax_window(
    store: &RemoteExtentStore,
    shm_region: &VirtioShmRegion,
    prefault: Option<Arc<dyn PrefaultGuestMemory>>,
) -> io::Result<()> {
    let base = std::ptr::NonNull::new(shm_region.host_addr as *mut u8)
        .ok_or_else(|| io::Error::other("the DAX window has no host memory"))?;
    let prefault = prefault.map(|prefault| -> Arc<dyn fxdax::Prefault> {
        Arc::new(WindowPrefault {
            guest_addr: shm_region.guest_addr,
            prefault,
        })
    });
    // SAFETY: The region's host memory is the device's, which only the guest
    // accesses, and outlives the store's connection.
    unsafe { store.create_dax_window_in_host_memory(base, shm_region.size as u64, prefault) }
        .map_err(|err| io::Error::other(format!("creating the DAX window: {err:#}")))
}
