// The extent-store device: its configuration, features and DAX window, and
// the worker that serves its requests once the driver activates it.

use std::cmp;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use fxdax::{ExtentStore, RemoteExtentStore};
use utils::eventfd::{EFD_NONBLOCK, EventFd};
use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
use vm_memory::{ByteValued, GuestMemoryMmap};

use super::dax::{PrefaultGuestMemory, create_dax_window};
use super::worker::ExtentWorker;
use super::{
    Config, MAX_EXTENT, MAX_ROOT, MAX_TRANSFER, NUM_QUEUES, QUEUE_CONFIG, QUEUE_SIZE, Resizing,
    TYPE_EXTENT, VIRTIO_EXTENT_F_DAX, VIRTIO_EXTENT_F_DAX_WRITE, VIRTIO_EXTENT_F_RESIZE,
    VIRTIO_EXTENT_F_WRITE_COMMIT,
};
use crate::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, InterruptTransport, QueueConfig,
    VirtioDevice, VirtioShmRegion,
};

pub struct Extent {
    store: Arc<RemoteExtentStore>,
    config: Config,
    /// The store's capacity, as the daemon changes it.
    resizing: Arc<Resizing>,
    avail_features: u64,
    acked_features: u64,
    device_state: DeviceState,
    shm_region: Option<VirtioShmRegion>,
    /// Whether the DAX window is created, which it is on the first activation.
    dax_window: bool,
    /// Maps the DAX window's pages for the guest as the store maps them.
    prefault: Option<Arc<dyn PrefaultGuestMemory>>,
    worker: Option<JoinHandle<DeviceQueue>>,
    worker_stopfd: EventFd,
}

impl Extent {
    /// A device tagged `tag` serving the extent store at `path`, which is
    /// created, holding at most `capacity` bytes, if it doesn't exist, in
    /// fxdax's extent daemon listening at `extentd`, which shares its memory
    /// with the host's other VMs.
    pub fn new(tag: &str, path: &Path, capacity: u64, extentd: &Path) -> io::Result<Self> {
        let store = RemoteExtentStore::connect(extentd, path, capacity).map_err(|err| {
            io::Error::other(format!(
                "opening extent store {} in {}: {err:#}",
                path.display(),
                extentd.display()
            ))
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
            capacity: 0,
            asked: 0,
        };
        let resizing = Arc::new(Resizing::default());
        let capacity = store.info().map_err(|err| io::Error::other(format!("{err:#}")))?.capacity;
        resizing.capacity.store(capacity, std::sync::atomic::Ordering::Release);
        let changes = resizing.clone();
        store.on_capacity(move |capacity, ask| changes.changed(capacity, ask));
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
            resizing,
            avail_features: (1 << VIRTIO_F_VERSION_1)
                | (1 << VIRTIO_EXTENT_F_WRITE_COMMIT)
                | (1 << VIRTIO_EXTENT_F_RESIZE),
            acked_features: 0,
            device_state: DeviceState::Inactive,
            shm_region: None,
            dax_window: false,
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
    pub fn set_prefault(&mut self, prefault: Arc<dyn PrefaultGuestMemory>) {
        self.prefault = Some(prefault);
    }

    /// Creates the store's DAX window in the shared memory region, unless
    /// there is none, or it exists.
    fn create_dax_window(&mut self) -> io::Result<()> {
        let Some(shm_region) = &self.shm_region else {
            return Ok(());
        };
        if !self.dax_window {
            create_dax_window(&self.store, shm_region, self.prefault.clone())?;
            self.dax_window = true;
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
        use std::sync::atomic::Ordering;
        let config = Config {
            capacity: self.resizing.capacity.load(Ordering::Acquire).to_le(),
            asked: self.resizing.asked.load(Ordering::Acquire).to_le(),
            ..self.config
        };
        let config = config.as_slice();
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
            self.resizing.clone(),
            stop_fd,
        );
        self.worker = Some(worker.run());
        *self.resizing.interrupt.lock().unwrap() = Some(interrupt.clone());
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
