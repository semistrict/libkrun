// virtio-fs backend serving an fxdax (Fxfs) image.
//
// fxdax decodes the device's FUSE requests itself: the backend hands its
// server each descriptor chain as the chain's readable and writable buffers,
// as volatile slices of guest memory, which it only copies into and out of.
//
// The device's loop runs on the filesystem's own threads, where Fxfs runs: a
// request the guest notifies wakes that thread, which handles it, and its
// work for Fxfs completes there, without waking another thread
// (`FsWorker::serve_fxdax`). Requests are handled concurrently.
//
// A device serves the filesystem on an image, or one of its volumes: devices
// serving volumes of the same image share it, mounted once, until the last of
// them is dropped.
//
// DAX mappings map files' pages into the filesystem's DAX window, whose pages
// the kernel maps as the guest faults on them, and into the device's shared
// memory region too:
// - On macOS the region is demand paged: HVF passes the guest's faults there
//   to the window, and the kernel maps the window's pages into HVF's stage 2.
// - On Linux, KVM follows the region's host memory, which the filesystem takes
//   over: it maps the window's pages there, and serves the faults KVM takes
//   there itself. KVM would map each page for the guest on a fault of the
//   guest's, which nested virtualization makes slow: given a
//   `PrefaultGuestMemory`, the window has KVM map the pages as it maps them.

use std::collections::BTreeMap;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use futures::executor::block_on;
#[cfg(target_os = "macos")]
use fxdax::Protection;
use fxdax::fuse::Server;
use fxdax::{Errno, Filesystem, Image, Storage, VolatileSlice, VolatileSliceMut};
use vm_memory::{Address, GuestMemory, GuestMemoryMmap, GuestMemoryRegion, VolatileMemory};

use crate::virtio::VirtioShmRegion;
use crate::virtio::queue::DescriptorChain;

/// The fxdax server of a virtio-fs device.
pub struct FxdaxServer {
    server: Server,
    /// What passes the guest's faults on the DAX window to it, while the guest
    /// may fault on it.
    _dax_faults: Option<DaxFaults>,
    /// The image whose volume the device serves, which it shares with the
    /// devices serving its other volumes, and closes once they are all dropped.
    _image: Option<Arc<Image>>,
}

/// The images whose volumes devices serve, by their canonical paths.
static IMAGES: Mutex<BTreeMap<PathBuf, Weak<Image>>> = Mutex::new(BTreeMap::new());

/// The image at `path`, mounted, which devices serving its volumes share.
fn shared_image(path: &Path) -> io::Result<Arc<Image>> {
    let path = path.canonicalize()?;
    let mut images = IMAGES.lock().unwrap();
    images.retain(|_, image| image.strong_count() > 0);
    if let Some(image) = images.get(&path).and_then(Weak::upgrade) {
        return Ok(image);
    }
    let image = Storage::open(&path)
        .and_then(|storage| block_on(Image::mount(storage)))
        .map(Arc::new)
        .map_err(|err| {
            io::Error::other(format!("mounting fxdax image {}: {err:#}", path.display()))
        })?;
    images.insert(path, Arc::downgrade(&image));
    Ok(image)
}

/// The volume `name` of `image`, which is created if it doesn't exist.
fn open_volume(image: &Image, name: &str) -> Result<Filesystem, fxdax::Error> {
    match block_on(image.open_volume(name)) {
        Err(err) if err.errno() == Some(Errno::ENOENT) => block_on(image.create_volume(name)),
        result => result,
    }
}

/// What passes the guest's faults on a DAX window to it, while the guest may
/// fault on it.
#[cfg(target_os = "macos")]
pub(crate) type DaxFaults = hvf::DemandPagedRegion;
/// The filesystem serves the faults on the window's memory itself.
#[cfg(target_os = "linux")]
pub(crate) type DaxFaults = ();

/// What a DAX window is created for: an fxdax filesystem, or an extent
/// store, which map into it what the guest asks for.
pub(crate) trait DaxOwner {
    /// Creates the owner's window, for a hypervisor whose `second_stage` the
    /// VMM maintains.
    #[cfg(target_os = "macos")]
    fn create_dax_window(
        &self,
        size: u64,
        second_stage: Option<Arc<dyn fxdax::SecondStage>>,
    ) -> Result<fxdax::DaxWindow, fxdax::Error>;

    /// Creates the owner's window in host memory the hypervisor follows.
    ///
    /// # Safety
    ///
    /// As `Filesystem::create_dax_window_in_host_memory`'s.
    #[cfg(target_os = "linux")]
    unsafe fn create_dax_window_in_host_memory(
        &self,
        base: std::ptr::NonNull<u8>,
        size: u64,
        prefault: Option<Arc<dyn fxdax::Prefault>>,
    ) -> Result<fxdax::DaxWindow, fxdax::Error>;
}

macro_rules! dax_owner {
    ($owner:ty) => {
        impl DaxOwner for $owner {
            #[cfg(target_os = "macos")]
            fn create_dax_window(
                &self,
                size: u64,
                second_stage: Option<Arc<dyn fxdax::SecondStage>>,
            ) -> Result<fxdax::DaxWindow, fxdax::Error> {
                <$owner>::create_dax_window(self, size, second_stage)
            }

            #[cfg(target_os = "linux")]
            unsafe fn create_dax_window_in_host_memory(
                &self,
                base: std::ptr::NonNull<u8>,
                size: u64,
                prefault: Option<Arc<dyn fxdax::Prefault>>,
            ) -> Result<fxdax::DaxWindow, fxdax::Error> {
                // SAFETY: The caller's guarantees are the owner's.
                unsafe { <$owner>::create_dax_window_in_host_memory(self, base, size, prefault) }
            }
        }
    };
}

dax_owner!(Filesystem);
dax_owner!(fxdax::ExtentStore);

impl FxdaxServer {
    /// Opens the fxdax image at `image`, and serves the filesystem on it, or
    /// its volume `volume`, serving DAX mappings in `shm_region`, which
    /// `prefault`, if any, maps for the guest ahead of its accesses.
    pub fn open(
        image: &Path,
        volume: Option<&str>,
        shm_region: Option<&VirtioShmRegion>,
        #[cfg(target_os = "linux")] prefault: Option<Arc<dyn PrefaultGuestMemory>>,
    ) -> io::Result<Self> {
        let (fs, _image) = match volume {
            None => {
                let fs = Storage::open(image)
                    .and_then(|storage| block_on(Filesystem::mount(storage)))
                    .map_err(|err| {
                        io::Error::other(format!(
                            "mounting fxdax image {}: {err:#}",
                            image.display()
                        ))
                    })?;
                (fs, None)
            }
            Some(volume) => {
                let shared = shared_image(image)?;
                let fs = open_volume(&shared, volume).map_err(|err| {
                    io::Error::other(format!(
                        "mounting volume {volume} of fxdax image {}: {err:#}",
                        image.display()
                    ))
                })?;
                (fs, Some(shared))
            }
        };
        let _dax_faults = shm_region
            .map(|shm_region| {
                dax_window(
                    &fs,
                    shm_region,
                    #[cfg(target_os = "linux")]
                    prefault,
                )
            })
            .transpose()?;
        Ok(Self {
            server: Server::new(fs),
            _dax_faults,
            _image,
        })
    }

    /// What runs the device's loop on the filesystem's own threads (see
    /// `FsWorker::serve_fxdax`).
    pub fn spawner(&self) -> fxdax::Spawner {
        self.server.filesystem().spawner()
    }

    /// Handles the request in `chain`, and returns the length of its reply.
    pub async fn handle(&self, mem: &GuestMemoryMmap, chain: DescriptorChain<'_>) -> usize {
        let (request, mut reply) = match buffers(mem, chain) {
            Ok(buffers) => buffers,
            Err(err) => {
                error!("fxdax: a descriptor chain is not guest memory: {err}");
                return 0;
            }
        };
        self.server
            .handle(&request, &mut reply)
            .await
            .unwrap_or_else(|err| {
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

/// Gives fxdax's in-process kernel, which files' pages live in, `memory_bytes`
/// of memory, with its memory pressure thresholds scaled to it, once. Fails
/// with `EBUSY` if set already or once a device has started the kernel, and
/// with `EINVAL` for too little memory.
pub fn set_kernel_memory(memory_bytes: u64) -> Result<(), i32> {
    fxdax::kernel::configure(fxdax::kernel::Config::with_memory(memory_bytes))
        .map_err(|err| err.errno().map_or(libc::EINVAL, Errno::raw))
}

/// Maps guest memory for the guest ahead of its accesses, as KVM's
/// `KVM_PRE_FAULT_MEMORY` does, rather than on a fault on each page.
#[cfg(target_os = "linux")]
pub trait PrefaultGuestMemory: Send + Sync {
    /// Maps the `len` bytes of guest memory at `guest_addr`, which the host
    /// has mapped, for the guest. It must not wait for the guest.
    fn prefault(&self, guest_addr: u64, len: u64);
}

/// Has `prefault` map the pages the window maps, at its guest address.
#[cfg(target_os = "linux")]
struct WindowPrefault {
    guest_addr: u64,
    prefault: Arc<dyn PrefaultGuestMemory>,
}

#[cfg(target_os = "linux")]
impl fxdax::Prefault for WindowPrefault {
    fn prefault(&self, offset: u64, len: u64) {
        self.prefault.prefault(self.guest_addr + offset, len);
    }
}

/// Creates `fs`'s DAX window in the host memory of `shm_region`, which KVM
/// follows: the filesystem maps the window's pages there, and serves the
/// faults KVM takes on the rest. With `prefault`, KVM maps the pages for the
/// guest as the window maps them.
#[cfg(target_os = "linux")]
pub(crate) fn dax_window(
    fs: &impl DaxOwner,
    shm_region: &VirtioShmRegion,
    prefault: Option<Arc<dyn PrefaultGuestMemory>>,
) -> io::Result<DaxFaults> {
    let base = std::ptr::NonNull::new(shm_region.host_addr as *mut u8)
        .ok_or_else(|| io::Error::other("the DAX window has no host memory"))?;
    let prefault = prefault.map(|prefault| -> Arc<dyn fxdax::Prefault> {
        Arc::new(WindowPrefault {
            guest_addr: shm_region.guest_addr,
            prefault,
        })
    });
    // SAFETY: The region's host memory is the device's, which only the guest
    // accesses, and outlives the filesystem.
    unsafe { fs.create_dax_window_in_host_memory(base, shm_region.size as u64, prefault) }
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
pub(crate) fn dax_window(
    fs: &impl DaxOwner,
    shm_region: &VirtioShmRegion,
) -> io::Result<DaxFaults> {
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
