#[cfg(not(feature = "aws-nitro"))]
use devices::virtio::fs::virtual_entry::VirtualDirEntry;

/// What a virtio-fs device serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsSource {
    /// Only virtual entries, no host storage.
    Virtual,
    /// A host directory, passed through.
    Directory(String),
    /// A RedoxFS image file, with DAX support.
    #[cfg(feature = "redoxfs")]
    RedoxFsImage(String),
}

#[derive(Clone, Debug)]
pub struct FsDeviceConfig {
    pub fs_id: String,
    /// What the device serves.
    pub source: FsSource,
    pub shm_size: Option<usize>,
    pub read_only: bool,
    #[cfg(not(feature = "aws-nitro"))]
    pub virtual_entries: Vec<VirtualDirEntry>,
}
