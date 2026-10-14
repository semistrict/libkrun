// The virtio extent-store device: fxdax's backlog doc-4, which specifies it.
//
// A guest that runs Fxfs itself stores its filesystem through this device,
// as extents its Fxfs names: immutable pieces of data, written once and
// sealed by the next commit, freed whole, and a root naming where the
// filesystem starts. The host keeps them in an fxdax extent store
// (`fxdax::ExtentStore`) on local disk, and maps sealed extents into the
// device's DAX window from fxdax's cache of their pages.

mod device;
mod worker;

pub use self::device::Extent;

use vm_memory::ByteValued;

use super::QueueConfig;

/// The device's ID: "FX", outside the IDs OASIS assigns.
pub const TYPE_EXTENT: u32 = 0x4658;

/// Shared memory region 0 is a DAX window, and `MAP`/`UNMAP` work.
pub const VIRTIO_EXTENT_F_DAX: u32 = 0;
/// `MAP` maps open extents writably.
pub const VIRTIO_EXTENT_F_DAX_WRITE: u32 = 1;

pub const VIRTIO_EXTENT_T_READ: u32 = 1;
pub const VIRTIO_EXTENT_T_WRITE: u32 = 2;
pub const VIRTIO_EXTENT_T_FREE: u32 = 3;
pub const VIRTIO_EXTENT_T_MAP: u32 = 4;
pub const VIRTIO_EXTENT_T_UNMAP: u32 = 5;
pub const VIRTIO_EXTENT_T_COMMIT: u32 = 6;
pub const VIRTIO_EXTENT_T_ROOT: u32 = 7;
pub const VIRTIO_EXTENT_T_INFO: u32 = 8;

/// A `WRITE`'s flag: a barrier precedes it, as if a `COMMIT` did.
pub const VIRTIO_EXTENT_WRITE_BARRIER: u32 = 1 << 0;
/// A `COMMIT`'s flags: it completes once durable; it sets the root.
pub const VIRTIO_EXTENT_COMMIT_DURABLE: u32 = 1 << 0;
pub const VIRTIO_EXTENT_COMMIT_ROOT: u32 = 1 << 1;
/// A `MAP`'s flag: writable, which needs `DAX_WRITE`.
pub const VIRTIO_EXTENT_MAP_WRITE: u32 = 1 << 0;

const NUM_QUEUES: usize = 1;
const QUEUE_SIZE: u16 = 256;
static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [QueueConfig::new(QUEUE_SIZE); NUM_QUEUES];

/// The most bytes one `READ` or `WRITE` moves.
const MAX_TRANSFER: u32 = 8 << 20;
/// The most bytes the root holds.
const MAX_ROOT: u32 = 4096;
/// The most bytes one extent holds.
const MAX_EXTENT: u64 = fxdax::ExtentStore::MAX_EXTENT;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ExtentIdLe {
    pub branch: u64,
    pub sequence: u64,
}

/// Every request's header.
#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct RequestHeader {
    pub type_: u32,
    pub flags: u32,
    /// The driver's own, for tracing.
    pub unique: u64,
}

/// Every request's response, the last bytes the device writes.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Response {
    /// 0, or an errno.
    pub status: u32,
    pub reserved: u32,
}

/// `READ`'s and `WRITE`'s arguments.
#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct IoArgs {
    pub id: ExtentIdLe,
    pub offset: u64,
    pub len: u32,
    pub reserved: u32,
}

/// `MAP`'s arguments.
#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct MapArgs {
    pub id: ExtentIdLe,
    pub offset: u64,
    pub len: u64,
    pub window_offset: u64,
}

/// `UNMAP`'s arguments.
#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct UnmapArgs {
    pub window_offset: u64,
    pub len: u64,
}

/// `COMMIT`'s arguments.
#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct CommitArgs {
    pub root_len: u32,
    pub reserved: u32,
}

/// What follows the root `ROOT` writes.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct RootLen {
    pub len: u32,
    pub reserved: u32,
}

/// What `INFO` writes.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Info {
    pub capacity: u64,
    pub used: u64,
    pub available: u64,
    pub next_sequence: u64,
}

/// The device's configuration space.
#[derive(Copy, Clone, Debug)]
#[repr(C, packed)]
pub struct Config {
    /// The filesystem's name, NUL-padded: the mount's source.
    pub tag: [u8; 36],
    pub num_request_queues: u32,
    pub block_size: u32,
    pub map_alignment: u32,
    pub max_transfer: u32,
    pub max_segments: u32,
    pub max_root: u32,
    pub branch: u64,
    pub max_extent: u64,
}

// SAFETY: They are plain data, with no implicit padding.
unsafe impl ByteValued for ExtentIdLe {}
unsafe impl ByteValued for RequestHeader {}
unsafe impl ByteValued for Response {}
unsafe impl ByteValued for IoArgs {}
unsafe impl ByteValued for MapArgs {}
unsafe impl ByteValued for UnmapArgs {}
unsafe impl ByteValued for CommitArgs {}
unsafe impl ByteValued for RootLen {}
unsafe impl ByteValued for Info {}
unsafe impl ByteValued for Config {}

impl From<ExtentIdLe> for fxdax::ExtentId {
    fn from(id: ExtentIdLe) -> Self {
        Self {
            branch: u64::from_le(id.branch),
            sequence: u64::from_le(id.sequence),
        }
    }
}
