// RAM capture and restore helpers.
//
// Capture: the first snapshot writes guest memory into pages.img. Later
// snapshots clone the previous pages.img and patch only dirty RAM blocks. The
// offsets and region descriptors are recorded in vmstate so restore can rebuild
// the same mapping.
//
// Restore: map pages.img with MAP_PRIVATE so RAM is demand-paged and guest
// writes go to private COW pages. Capture later patches only dirty blocks back
// into the next cloned pages.img.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use hvf::DirtyBlock;
use serde::{Deserialize, Serialize};
use vm_memory::mmap::{GuestRegionMmap, MmapRegionBuilder};
use vm_memory::{
    Address, Bytes, FileOffset, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion,
};

use super::{pages_img_path, Result, SnapshotError};

/// Description of one guest memory region as preserved in the snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RamRegion {
    pub guest_addr: u64,
    pub size: u64,
    /// Offset within `pages.img`.
    pub file_offset: u64,
}

/// Snapshot of the guest's memory layout. The actual page contents live in
/// the sibling pages.img file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RamLayout {
    pub regions: Vec<RamRegion>,
}

/// Create a sparse zero-filled pages.img and patch the dirtied RAM blocks.
/// Untouched guest RAM remains represented by filesystem holes.
pub fn write_sparse_pages_img(
    mem: &GuestMemoryMmap,
    dir: &Path,
    dirty_blocks: &[DirtyBlock],
) -> Result<RamLayout> {
    let layout = layout_from_memory(mem);
    let path = pages_img_path(dir);
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    let len = layout
        .regions
        .iter()
        .map(|region| region.file_offset + region.size)
        .max()
        .unwrap_or(0);
    file.set_len(len)?;
    patch_dirty_blocks(mem, &file, &layout, dirty_blocks)?;
    file.sync_all()?;
    Ok(layout)
}

pub fn clone_and_patch_dirty_pages_img(
    mem: &GuestMemoryMmap,
    previous_dir: &Path,
    stage_dir: &Path,
    dirty_blocks: &[DirtyBlock],
) -> Result<RamLayout> {
    let layout = layout_from_memory(mem);
    let previous_pages = pages_img_path(previous_dir);
    let stage_pages = pages_img_path(stage_dir);
    clone_pages_img(&previous_pages, &stage_pages)?;

    let file = OpenOptions::new().write(true).open(&stage_pages)?;
    patch_dirty_blocks(mem, &file, &layout, dirty_blocks)?;
    file.sync_all()?;
    Ok(layout)
}

fn patch_dirty_blocks(
    mem: &GuestMemoryMmap,
    file: &File,
    layout: &RamLayout,
    dirty_blocks: &[DirtyBlock],
) -> Result<()> {
    let mut buf = vec![0u8; hvf::DIRTY_BLOCK_SIZE as usize];
    for block in dirty_blocks {
        let Some(file_offset) = guest_addr_to_file_offset(&layout, block.guest_addr) else {
            continue;
        };
        let size = block.size as usize;
        mem.read_slice(&mut buf[..size], GuestAddress(block.guest_addr))
            .map_err(|e| {
                SnapshotError::Io(std::io::Error::other(format!(
                    "read dirty block 0x{:x}: {e:?}",
                    block.guest_addr
                )))
            })?;
        file.write_all_at(&buf[..size], file_offset)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_pages_img(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src = CString::new(src.as_os_str().as_bytes())?;
    let dst = CString::new(dst.as_os_str().as_bytes())?;
    let ret = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn clone_pages_img(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

fn layout_from_memory(mem: &GuestMemoryMmap) -> RamLayout {
    let mut layout = RamLayout {
        regions: Vec::new(),
    };
    let mut cursor: u64 = 0;
    for region in mem.iter() {
        let start = region.start_addr().raw_value();
        let size = region.len();
        layout.regions.push(RamRegion {
            guest_addr: start,
            size,
            file_offset: cursor,
        });
        cursor += size;
    }
    layout
}

fn guest_addr_to_file_offset(layout: &RamLayout, guest_addr: u64) -> Option<u64> {
    for region in &layout.regions {
        if guest_addr >= region.guest_addr && guest_addr < region.guest_addr + region.size {
            return Some(region.file_offset + (guest_addr - region.guest_addr));
        }
    }
    None
}

/// Build private COW guest RAM backed by `<dir>/pages.img`.
pub fn restore_pages_img(dir: &Path, layout: &RamLayout) -> Result<GuestMemoryMmap> {
    let file = Arc::new(File::open(pages_img_path(dir))?);
    let mut regions = Vec::new();
    for r in &layout.regions {
        let mapping = MmapRegionBuilder::new(r.size as usize)
            .with_file_offset(FileOffset::from_arc(file.clone(), r.file_offset))
            .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
            .with_mmap_flags(libc::MAP_PRIVATE | libc::MAP_NORESERVE)
            .build()
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("{e:?}"))))?;
        let region = GuestRegionMmap::new(mapping, GuestAddress(r.guest_addr))
            .ok_or_else(|| SnapshotError::Io(std::io::Error::other("invalid guest region")))?;
        regions.push(region);
    }
    GuestMemoryMmap::from_regions(regions)
        .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("{e:?}"))))
}
