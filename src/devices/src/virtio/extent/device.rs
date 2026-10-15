// The extent-store device: its configuration, features and DAX window, and
// the worker that serves its requests once the driver activates it.

use std::cmp;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use fxdax::{ExtentStore, Extents};
use utils::eventfd::{EFD_NONBLOCK, EventFd};
use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
use vm_memory::{ByteValued, GuestMemoryMmap};

use super::worker::ExtentWorker;
use super::{
    Config, MAX_EXTENT, MAX_ROOT, MAX_TRANSFER, NUM_QUEUES, QUEUE_CONFIG, QUEUE_SIZE, TYPE_EXTENT,
    VIRTIO_EXTENT_F_DAX, VIRTIO_EXTENT_F_DAX_WRITE,
};
#[cfg(target_os = "linux")]
use crate::virtio::fs::PrefaultGuestMemory;
use crate::virtio::fs::{DaxFaults, dax_window};
use crate::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, InterruptTransport, QueueConfig,
    VirtioDevice, VirtioShmRegion,
};

pub struct Extent {
    store: Arc<Extents>,
    config: Config,
    avail_features: u64,
    acked_features: u64,
    device_state: DeviceState,
    shm_region: Option<VirtioShmRegion>,
    /// What passes the guest's faults on the DAX window to the store, once
    /// the window is created, which it is on the first activation.
    dax_faults: Option<DaxFaults>,
    /// Maps the DAX window's pages for the guest as the store maps them.
    #[cfg(target_os = "linux")]
    prefault: Option<Arc<dyn PrefaultGuestMemory>>,
    worker: Option<JoinHandle<DeviceQueue>>,
    worker_stopfd: EventFd,
}

impl Extent {
    /// A device tagged `tag` serving the extent store at `path`, which is
    /// created, holding at most `capacity` bytes, if it doesn't exist: in this
    /// process, or, where FXDAX_EXTENTD names its socket, in fxdax's extent
    /// daemon, which shares its memory with the host's other VMs.
    pub fn new(tag: &str, path: &Path, capacity: u64) -> io::Result<Self> {
        let daemon = std::env::var_os("FXDAX_EXTENTD").filter(|socket| !socket.is_empty());
        let store = Extents::open_or_create(path, capacity, daemon.as_deref().map(Path::new))
        .map_err(|err| {
            io::Error::other(format!("opening extent store {}: {err:#}", path.display()))
        })?;
        let mut config = Config {
            tag: [0; 36],
            num_request_queues: (NUM_QUEUES as u32).to_le(),
            block_size: ExtentStore::BLOCK_SIZE.to_le(),
            map_alignment: (fxdax::kernel::page_size() as u32).to_le(),
            max_transfer: MAX_TRANSFER.to_le(),
            max_segments: (QUEUE_SIZE as u32 - 2).to_le(),
            max_root: MAX_ROOT.to_le(),
            branch: store.branch().to_le(),
            max_extent: MAX_EXTENT.to_le(),
        };
        let tag = tag.as_bytes();
        if tag.len() > config.tag.len() {
            return Err(io::Error::other(format!(
                "the tag {tag:?} is longer than {} bytes",
                config.tag.len()
            )));
        }
        config.tag[..tag.len()].copy_from_slice(tag);
        Ok(Self {
            store: Arc::new(store),
            config,
            avail_features: 1 << VIRTIO_F_VERSION_1,
            acked_features: 0,
            device_state: DeviceState::Inactive,
            shm_region: None,
            dax_faults: None,
            #[cfg(target_os = "linux")]
            prefault: None,
            worker: None,
            worker_stopfd: EventFd::new(EFD_NONBLOCK)?,
        })
    }

    /// Makes `shm_region` the device's DAX window.
    pub fn set_shm_region(&mut self, shm_region: VirtioShmRegion) {
        self.shm_region = Some(shm_region);
        // The store maps open extents writably too (doc-4's DAX_WRITE).
        self.avail_features |= (1 << VIRTIO_EXTENT_F_DAX) | (1 << VIRTIO_EXTENT_F_DAX_WRITE);
    }

    /// Has `prefault` map the DAX window's pages for the guest as the store
    /// maps them, rather than the guest faulting on each.
    #[cfg(target_os = "linux")]
    pub fn set_prefault(&mut self, prefault: Arc<dyn PrefaultGuestMemory>) {
        self.prefault = Some(prefault);
    }

    /// Creates the store's DAX window in the shared memory region, unless
    /// there is none, or it exists.
    fn create_dax_window(&mut self) -> io::Result<()> {
        let Some(shm_region) = &self.shm_region else {
            return Ok(());
        };
        if self.dax_faults.is_none() {
            self.dax_faults = Some(dax_window(
                &*self.store,
                shm_region,
                #[cfg(target_os = "linux")]
                self.prefault.clone(),
            )?);
        }
        Ok(())
    }
}

impl VirtioDevice for Extent {
    fn device_type(&self) -> u32 {
        TYPE_EXTENT
    }

    fn device_name(&self) -> &str {
        "extent"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &QUEUE_CONFIG
    }

    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config = self.config.as_slice();
        let len = config.len() as u64;
        if offset >= len {
            error!("extent device: reading config past its end");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            data.write_all(&config[offset as usize..cmp::min(end, len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "extent device: the driver wrote {} bytes of config at {offset:#x}",
            data.len()
        );
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn shm_region(&self) -> Option<&VirtioShmRegion> {
        self.shm_region.as_ref()
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        let [queue]: [_; NUM_QUEUES] = queues.try_into().map_err(|_| {
            error!("extent device: expected {NUM_QUEUES} queue(s)");
            ActivateError::BadActivate
        })?;
        self.create_dax_window().map_err(|err| {
            error!("extent device: {err}");
            ActivateError::BadActivate
        })?;
        let stop_fd = self.worker_stopfd.try_clone().map_err(|err| {
            error!("extent device: {err}");
            ActivateError::BadActivate
        })?;
        let worker = ExtentWorker::new(
            queue,
            interrupt.clone(),
            mem.clone(),
            self.store.clone(),
            stop_fd,
        );
        self.worker = Some(worker.run());
        self.device_state = DeviceState::Activated(mem, interrupt);
        Ok(())
    }

    fn reset(&mut self) -> bool {
        if let Some(worker) = self.worker.take() {
            let _ = self.worker_stopfd.write(1);
            if let Err(err) = worker.join() {
                error!("extent device: waiting for the worker: {err:?}");
            }
        }
        self.device_state = DeviceState::Inactive;
        true
    }
}
