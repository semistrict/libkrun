// virtio-fs backend serving an fxdax (Fxfs) image.
//
// fxdax decodes the device's FUSE requests itself: the backend hands its
// server each descriptor chain as the chain's readable and writable buffers,
// as volatile slices of guest memory, which it only copies into and out of.
//
// DAX mappings map files' pages into the filesystem's DAX window, whose pages
// the kernel maps as the guest faults on them, and into the device's shared
// memory region too:
// - On macOS the region is demand paged: HVF passes the guest's faults there
//   to the window, and the kernel maps the window's pages into HVF's stage 2.
// - On Linux, KVM follows the region's host memory, which the filesystem takes
//   over: it maps the window's pages there, and serves the faults KVM takes
//   there itself.

use std::io;
use std::ops::Deref;
use std::path::Path;

use futures::executor::block_on;
#[cfg(target_os = "macos")]
use fxdax::Protection;
use fxdax::fuse::Server;
use fxdax::{Filesystem, Storage, VolatileSlice, VolatileSliceMut};
use vm_memory::{Address, GuestMemory, GuestMemoryMmap, GuestMemoryRegion, VolatileMemory};

use crate::virtio::VirtioShmRegion;
use crate::virtio::queue::DescriptorChain;

/// The fxdax server of a virtio-fs device.
pub struct FxdaxServer {
    server: Server,
    /// What passes the guest's faults on the DAX window to it, while the guest
    /// may fault on it.
    _dax_faults: Option<DaxFaults>,
}

#[cfg(target_os = "macos")]
type DaxFaults = hvf::DemandPagedRegion;
/// The filesystem serves the faults on the window's memory itself.
#[cfg(target_os = "linux")]
type DaxFaults = ();

impl FxdaxServer {
    /// Opens the fxdax image at `image`, serving DAX mappings in `shm_region`.
    pub fn open(image: &Path, shm_region: Option<&VirtioShmRegion>) -> io::Result<Self> {
        let fs = Storage::open(image)
            .and_then(|storage| block_on(Filesystem::mount(storage)))
            .map_err(|err| {
                io::Error::other(format!("mounting fxdax image {}: {err:#}", image.display()))
            })?;
        let _dax_faults = shm_region
            .map(|shm_region| dax_window(&fs, shm_region))
            .transpose()?;
        Ok(Self {
            server: Server::new(fs),
            _dax_faults,
        })
    }

    /// Handles the request in `chain`, and returns the length of its reply.
    pub fn handle_chain(&self, mem: &GuestMemoryMmap, chain: DescriptorChain) -> usize {
        let (request, mut reply) = match buffers(mem, chain) {
            Ok(buffers) => buffers,
            Err(err) => {
                error!("fxdax: a descriptor chain is not guest memory: {err}");
                return 0;
            }
        };
        block_on(self.server.handle(&request, &mut reply)).unwrap_or_else(|err| {
            error!("fxdax: {err:#}");
            0
        })
    }
}

/// The readable and writable buffers of `chain`, in guest memory, mapped in
/// the VMM. Writable ones follow readable ones, as virtio requires.
fn buffers<'m>(
    mem: &'m GuestMemoryMmap,
    chain: DescriptorChain<'m>,
) -> io::Result<(Vec<VolatileSlice<'m>>, Vec<VolatileSliceMut<'m>>)> {
    let (mut readable, mut writable) = (Vec::new(), Vec::new());
    for desc in chain.into_iter() {
        let region = mem
            .find_region(desc.addr)
            .ok_or_else(|| io::Error::other(format!("no memory at {:#x}", desc.addr.0)))?;
        let offset = desc.addr.unchecked_offset_from(region.start_addr()) as usize;
        let slice = region
            .deref()
            .get_slice(offset, desc.len as usize)
            .map_err(io::Error::other)?;
        let ptr = slice.ptr_guard_mut().as_ptr();
        if desc.is_write_only() {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let _ = hvf::mark_dirty_ranges(&[(desc.addr.raw_value(), desc.len as u64)]);
            }
            // SAFETY: The descriptor's memory is guest memory, mapped for the
            // VM's lifetime, which fxdax only copies into.
            writable.push(unsafe { VolatileSliceMut::from_raw_parts(ptr, desc.len as usize) });
        } else {
            // SAFETY: As above; fxdax only copies out of it.
            readable.push(unsafe { VolatileSlice::from_raw_parts(ptr, desc.len as usize) });
        }
    }
    Ok((readable, writable))
}

/// Creates `fs`'s DAX window in the host memory of `shm_region`, which KVM
/// follows: the filesystem maps the window's pages there, and serves the
/// faults KVM takes on the rest.
#[cfg(target_os = "linux")]
fn dax_window(fs: &Filesystem, shm_region: &VirtioShmRegion) -> io::Result<DaxFaults> {
    let base = std::ptr::NonNull::new(shm_region.host_addr as *mut u8)
        .ok_or_else(|| io::Error::other("the DAX window has no host memory"))?;
    // SAFETY: The region's host memory is the device's, which only the guest
    // accesses, and outlives the filesystem.
    unsafe { fs.create_dax_window_in_host_memory(base, shm_region.size as u64) }
        .map_err(dax_error)?;
    Ok(())
}

/// An error creating the DAX window.
fn dax_error(err: fxdax::Error) -> io::Error {
    io::Error::other(format!("creating the DAX window: {err:#}"))
}

/// Creates `fs`'s DAX window as the guest memory of `shm_region`, which HVF
/// demand pages: the guest's faults there go to the window, and the kernel
/// maps the window's pages into the guest's memory as it maps them into the
/// window's host memory.
#[cfg(target_os = "macos")]
fn dax_window(fs: &Filesystem, shm_region: &VirtioShmRegion) -> io::Result<DaxFaults> {
    let guest_addr = shm_region.guest_addr;
    let size = shm_region.size as u64;
    let window = fs
        .create_dax_window(
            size,
            Some(std::sync::Arc::new(HvfSecondStage { guest_addr })),
        )
        .map_err(dax_error)?;
    hvf::DemandPagedRegion::new(
        guest_addr,
        size,
        std::sync::Arc::new(move |offset, write| {
            let access = if write {
                fxdax::Access::Write
            } else {
                fxdax::Access::Read
            };
            block_on(window.fault(offset, access)).is_ok()
        }),
    )
    .map_err(|err| io::Error::other(format!("demand paging the DAX window: {err}")))
}

/// HVF's second stage translation of a DAX window at `guest_addr`.
#[cfg(target_os = "macos")]
struct HvfSecondStage {
    guest_addr: u64,
}

#[cfg(target_os = "macos")]
impl HvfSecondStage {
    /// HVF memory flags for `protection`. The guest may execute what it may
    /// read, such as programs on the filesystem.
    fn flags(protection: Protection) -> hvf::bindings::hv_memory_flags_t {
        use hvf::bindings::{HV_MEMORY_EXEC, HV_MEMORY_READ, HV_MEMORY_WRITE};
        let flags = match protection {
            Protection::None => 0,
            Protection::Read => HV_MEMORY_READ | HV_MEMORY_EXEC,
            Protection::ReadWrite => HV_MEMORY_READ | HV_MEMORY_EXEC | HV_MEMORY_WRITE,
        };
        flags.into()
    }
}

#[cfg(target_os = "macos")]
impl fxdax::SecondStage for HvfSecondStage {
    fn map(&self, offset: u64, host_addr: *mut u8, len: u64, protection: Protection) {
        let guest_addr = self.guest_addr + offset;
        hvf::map_guest_memory(host_addr as u64, guest_addr, len, Self::flags(protection))
            .unwrap_or_else(|err| panic!("mapping DAX window memory at {guest_addr:#x}: {err}"));
    }

    fn protect(&self, offset: u64, len: u64, protection: Protection) {
        let guest_addr = self.guest_addr + offset;
        hvf::protect_guest_memory(guest_addr, len, Self::flags(protection))
            .unwrap_or_else(|err| panic!("protecting DAX window memory at {guest_addr:#x}: {err}"));
    }

    fn unmap(&self, offset: u64, len: u64) {
        let guest_addr = self.guest_addr + offset;
        hvf::unmap_guest_memory(guest_addr, len)
            .unwrap_or_else(|err| panic!("unmapping DAX window memory at {guest_addr:#x}: {err}"));
    }
}
