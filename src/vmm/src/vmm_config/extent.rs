/// A virtio extent-store device: the store of extents that a guest running Fxfs
/// itself keeps its filesystem in (fxdax's backlog doc-4).
#[derive(Clone, Debug)]
pub struct ExtentDeviceConfig {
    /// The filesystem's name, which the guest mounts it by.
    pub tag: String,
    /// The extent store's directory, created if it doesn't exist.
    pub path: String,
    /// The most bytes a store created holds.
    pub capacity: u64,
    /// The size of the DAX window, if it has one.
    pub shm_size: Option<usize>,
    /// The socket of fxdax's extent daemon, which serves the store.
    pub extentd: String,
}
