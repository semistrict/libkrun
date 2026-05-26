// RAM capture and restore helpers.
//
// Capture: walk each guest memory region, msync host-backed pages, then write
// the region's bytes to pages.img at a fixed offset (offsets are assigned by
// region iteration order). Both the offsets and the region descriptors are
// recorded in the vmstate so restore can rebuild the same mapping.
//
// Restore: open pages.img read-only, build a file-backed GuestMemoryMmap so
// the kernel pages guest memory in lazily from the file when HVF or the
// guest first touches a page.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

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

/// Walk every region of `mem`, msync, and write the concatenated bytes to
/// `<dir>/pages.img`. Returns the recorded layout for the META section.
pub fn write_pages_img(mem: &GuestMemoryMmap, dir: &Path) -> Result<RamLayout> {
    let path = pages_img_path(dir);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;

    let mut layout = RamLayout {
        regions: Vec::new(),
    };
    let mut cursor: u64 = 0;
    for region in mem.iter() {
        let start = region.start_addr().raw_value();
        let size = region.len();

        // msync the host pages so writes from device DMA and the guest are
        // committed to physical memory (and to disk if the region is file-backed,
        // though normal regions aren't).
        if let Ok(host_addr) = mem.get_host_address(region.start_addr()) {
            unsafe {
                // MS_SYNC = 0x10 on darwin (matches Linux). Best-effort.
                let _ = libc::msync(host_addr as *mut libc::c_void, size as usize, libc::MS_SYNC);
            }
        }

        // Read region bytes into a buffer and write to pages.img.
        let mut buf = vec![0u8; size as usize];
        mem.read_slice(&mut buf, region.start_addr()).map_err(|e| {
            SnapshotError::Io(std::io::Error::other(format!(
                "read region 0x{start:x}: {e:?}"
            )))
        })?;
        file.write_all(&buf)?;

        layout.regions.push(RamRegion {
            guest_addr: start,
            size,
            file_offset: cursor,
        });
        cursor += size;
    }
    file.sync_all()?;
    Ok(layout)
}

/// Build a file-backed `GuestMemoryMmap` whose pages are demand-paged from
/// `<dir>/pages.img`. The host kernel handles the lazy page-in.
pub fn restore_pages_img(dir: &Path, layout: &RamLayout) -> Result<GuestMemoryMmap> {
    // Build anonymous (writable) RAM and read the snapshot into it. This
    // gives HVF a stable host-physical backing — MAP_PRIVATE copy-on-write
    // on file-backed memory races with HVF's stage-2 mapping on Apple
    // Silicon and can leave the guest seeing stale pages after the first
    // write. v1 trades demand paging for correctness; v2 can do the
    // userfaultfd-equivalent under a Mach exception handler.
    let ranges: Vec<(GuestAddress, usize)> = layout
        .regions
        .iter()
        .map(|r| (GuestAddress(r.guest_addr), r.size as usize))
        .collect();
    let mem = GuestMemoryMmap::from_ranges(&ranges)
        .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("{e:?}"))))?;

    load_pages_img_into(&mem, dir, layout)?;
    Ok(mem)
}

pub fn load_pages_img_into(mem: &GuestMemoryMmap, dir: &Path, layout: &RamLayout) -> Result<()> {
    let path = pages_img_path(dir);
    let mut file = File::open(&path)?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    for r in &layout.regions {
        file.seek(SeekFrom::Start(r.file_offset))?;
        let mut remaining = r.size as usize;
        let mut guest_offset: u64 = 0;
        while remaining > 0 {
            let chunk = remaining.min(buf.len());
            file.read_exact(&mut buf[..chunk])?;
            let addr = GuestAddress(r.guest_addr + guest_offset);
            mem.write_slice(&buf[..chunk], addr).map_err(|e| {
                SnapshotError::Io(std::io::Error::other(format!(
                    "write_slice 0x{:x}: {e:?}",
                    addr.0
                )))
            })?;
            guest_offset += chunk as u64;
            remaining -= chunk;
        }
    }
    Ok(())
}
