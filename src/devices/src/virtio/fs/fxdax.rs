// virtio-fs backend serving an fxdax (Fxfs) image.
//
// All filesystem semantics live in `fxdax::Filesystem`, which runs Fxfs on
// the in-process kernel; this adapter translates between it and the FUSE
// server.
//
// DAX mappings map files' pages into the filesystem's DAX window, whose pages
// the kernel maps as the guest faults on them, and into the device's shared
// memory region too:
// - On macOS the region is demand paged: HVF passes the guest's faults there
//   to the window, and the kernel maps the window's pages into HVF's stage 2.
// - On Linux, KVM follows the region's host memory, which the filesystem takes
//   over: it maps the window's pages there, and serves the faults KVM takes
//   there itself.

use std::ffi::CStr;
use std::io;
use std::mem;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use crossbeam_channel::Sender;
#[cfg(target_os = "macos")]
use fxdax::Protection;
use fxdax::{
    Attr, DirEntry as FxDirEntry, DirHandle, Errno, FileType, Filesystem, Owner, RenameFlags,
    SetAttr, SetTime, Storage,
};
#[cfg(target_os = "macos")]
use utils::worker_message::WorkerMessage;

use super::bindings;
use super::filesystem::{
    Context, DirEntry, Entry, Extensions, FileSystem, FsOptions, OpenOptions, SetattrValid,
    ZeroCopyReader, ZeroCopyWriter,
};
use super::fuse::{RemovemappingOne, SetupmappingFlags};
use crate::virtio::VirtioShmRegion;

type Inode = u64;
type Handle = u64;

/// The guest is the only client of the image, so nothing changes behind its
/// back and cached names and attributes stay valid.
const TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Lowest inode number `AugmentFs` may hand to virtual entries: fxdax inode
/// numbers are Fxfs object ids, which an unencrypted volume allocates
/// sequentially from small numbers.
pub const FIRST_VIRTUAL_INODE: u64 = 1 << 63;

pub struct FxdaxFs {
    fs: Filesystem,
    /// What passes the guest's faults on the DAX window to it, while the guest
    /// may fault on it.
    _dax_faults: Option<DaxFaults>,
}

#[cfg(target_os = "macos")]
type DaxFaults = hvf::DemandPagedRegion;
/// The filesystem serves the faults on the window's memory itself.
#[cfg(target_os = "linux")]
type DaxFaults = ();

/// Conversion of `fxdax` results for the guest.
trait Guest<T> {
    fn guest(self) -> io::Result<T>;
}

impl<T> Guest<T> for fxdax::Result<T> {
    fn guest(self) -> io::Result<T> {
        // fxdax's errno values are already Linux's.
        self.map_err(|errno| io::Error::from_raw_os_error(errno.raw()))
    }
}

fn errno(errno: Errno) -> io::Error {
    io::Error::from_raw_os_error(errno.raw())
}

fn owner(ctx: &Context) -> Owner {
    Owner {
        uid: ctx.uid,
        gid: ctx.gid,
    }
}

/// Seconds and nanoseconds of `time` since the Unix epoch, which fxdax's times
/// are not before.
fn timespec(time: SystemTime) -> (i64, i64) {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    (
        since_epoch.as_secs() as i64,
        since_epoch.subsec_nanos() as i64,
    )
}

fn stat(attr: &Attr) -> bindings::stat64 {
    // SAFETY: stat is plain data; all-zero is a valid value.
    let mut st: bindings::stat64 = unsafe { mem::zeroed() };
    let (atime, mtime, ctime) = (
        timespec(attr.atime),
        timespec(attr.mtime),
        timespec(attr.ctime),
    );
    st.st_ino = attr.inode.0 as _;
    st.st_mode = attr.mode() as _;
    st.st_nlink = attr.nlink as _;
    st.st_uid = attr.uid;
    st.st_gid = attr.gid;
    st.st_rdev = attr.rdev as _;
    st.st_size = attr.size as _;
    st.st_blocks = attr.blocks as _;
    st.st_blksize = attr.blksize as _;
    st.st_atime = atime.0 as _;
    st.st_atime_nsec = atime.1 as _;
    st.st_mtime = mtime.0 as _;
    st.st_mtime_nsec = mtime.1 as _;
    st.st_ctime = ctime.0 as _;
    st.st_ctime_nsec = ctime.1 as _;
    st
}

/// Permission bits of a stat's mode.
// st_mode is u16 on macOS and u32 on Linux.
#[allow(clippy::unnecessary_cast)]
fn perm(st: &bindings::stat64) -> u16 {
    (st.st_mode as u32 & 0o7777) as u16
}

fn entry(attr: &Attr) -> Entry {
    Entry {
        inode: attr.inode.0,
        generation: 0,
        attr: stat(attr),
        attr_flags: 0,
        attr_timeout: TIMEOUT,
        entry_timeout: TIMEOUT,
    }
}

fn set_time(
    valid: SetattrValid,
    set: SetattrValid,
    now: SetattrValid,
    secs: i64,
    nanos: i64,
) -> Option<SetTime> {
    if valid.contains(now) {
        Some(SetTime::Now)
    } else if valid.contains(set) {
        // Times before the epoch, which fxdax refuses, are before it here too.
        let time = match u64::try_from(secs) {
            Ok(secs) => UNIX_EPOCH + Duration::new(secs, nanos as u32),
            Err(_) => UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()),
        };
        Some(SetTime::At(time))
    } else {
        None
    }
}

fn is_dot(name: &[u8]) -> bool {
    name == b"." || name == b".."
}

impl FxdaxFs {
    /// Open the fxdax image at `image`, serving DAX mappings in `shm_region`.
    pub fn open(image: &Path, shm_region: Option<&VirtioShmRegion>) -> io::Result<Self> {
        let fs = Storage::open(image)
            .and_then(Filesystem::mount)
            .map_err(|err| {
                io::Error::other(format!("mounting fxdax image {}: {err:#}", image.display()))
            })?;
        let _dax_faults = shm_region
            .map(|shm_region| dax_window(&fs, shm_region))
            .transpose()?;
        Ok(Self { fs, _dax_faults })
    }

    fn truncate(&self, inode: Inode, kill_priv: bool) -> io::Result<()> {
        self.fs
            .setattr(
                inode.into(),
                &SetAttr {
                    size: Some(0),
                    kill_suid_sgid: kill_priv,
                    ..Default::default()
                },
            )
            .guest()
            .map(|_| ())
    }

    fn list<F>(&self, handle: Handle, offset: u64, mut add: F) -> io::Result<()>
    where
        F: FnMut(u64, &FxDirEntry) -> io::Result<usize>,
    {
        let entries = self.fs.readdir(DirHandle(handle), offset).guest()?;
        for (i, entry) in entries.iter().enumerate() {
            if add(offset + i as u64 + 1, entry)? == 0 {
                break;
            }
        }
        Ok(())
    }
}

impl FileSystem for FxdaxFs {
    type Inode = Inode;
    type Handle = Handle;

    fn init(&self, _capable: FsOptions) -> io::Result<FsOptions> {
        Ok(FsOptions::empty())
    }

    fn destroy(&self) {
        if let Err(err) = self.fs.unmount() {
            error!("fxdax: failed to write back: {err:?}");
        }
    }

    fn lookup(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        let attr = self.fs.lookup(parent.into(), name.to_bytes()).guest()?;
        Ok(entry(&attr))
    }

    fn forget(&self, _ctx: Context, inode: Inode, count: u64) {
        self.fs.forget(inode.into(), count);
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Option<Handle>,
    ) -> io::Result<(bindings::stat64, Duration)> {
        let attr = self.fs.getattr(inode.into()).guest()?;
        Ok((stat(&attr), TIMEOUT))
    }

    fn setattr(
        &self,
        _ctx: Context,
        inode: Inode,
        attr: bindings::stat64,
        _handle: Option<Handle>,
        valid: SetattrValid,
    ) -> io::Result<(bindings::stat64, Duration)> {
        let set = SetAttr {
            perm: valid.contains(SetattrValid::MODE).then_some(perm(&attr)),
            uid: valid.contains(SetattrValid::UID).then_some(attr.st_uid),
            gid: valid.contains(SetattrValid::GID).then_some(attr.st_gid),
            size: valid
                .contains(SetattrValid::SIZE)
                .then_some(attr.st_size as u64),
            atime: set_time(
                valid,
                SetattrValid::ATIME,
                SetattrValid::ATIME_NOW,
                attr.st_atime,
                attr.st_atime_nsec,
            ),
            mtime: set_time(
                valid,
                SetattrValid::MTIME,
                SetattrValid::MTIME_NOW,
                attr.st_mtime,
                attr.st_mtime_nsec,
            ),
            // FUSE_HANDLE_KILLPRIV_V2 rules
            kill_suid_sgid: valid.contains(SetattrValid::SIZE | SetattrValid::KILL_SUIDGID)
                || valid.intersects(SetattrValid::UID | SetattrValid::GID),
        };
        let attr = self.fs.setattr(inode.into(), &set).guest()?;
        Ok((stat(&attr), TIMEOUT))
    }

    fn readlink(&self, _ctx: Context, inode: Inode) -> io::Result<Vec<u8>> {
        self.fs.readlink(inode.into()).guest()
    }

    fn symlink(
        &self,
        _ctx: Context,
        linkname: &CStr,
        parent: Inode,
        name: &CStr,
        _extensions: Extensions,
    ) -> io::Result<Entry> {
        let attr = self
            .fs
            .symlink(parent.into(), name.to_bytes(), linkname.to_bytes())
            .guest()?;
        Ok(entry(&attr))
    }

    fn mknod(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        _extensions: Extensions,
    ) -> io::Result<Entry> {
        let mode = mode & !umask;
        // A mode without a type is a regular file's.
        let file_type = FileType::from_mode(mode).unwrap_or(FileType::File);
        let perm = (mode & 0o7777) as u16;
        let attr = self
            .fs
            .mknod(
                parent.into(),
                name.to_bytes(),
                file_type,
                perm,
                rdev,
                owner(&ctx),
            )
            .guest()?;
        Ok(entry(&attr))
    }

    fn mkdir(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
        _extensions: Extensions,
    ) -> io::Result<Entry> {
        let perm = ((mode & !umask) & 0o7777) as u16;
        let attr = self
            .fs
            .mkdir(parent.into(), name.to_bytes(), perm, owner(&ctx))
            .guest()?;
        Ok(entry(&attr))
    }

    fn unlink(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.fs.unlink(parent.into(), name.to_bytes()).guest()
    }

    fn rmdir(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.fs.rmdir(parent.into(), name.to_bytes()).guest()
    }

    fn rename(
        &self,
        _ctx: Context,
        olddir: Inode,
        oldname: &CStr,
        newdir: Inode,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        self.fs
            .rename(
                olddir.into(),
                oldname.to_bytes(),
                newdir.into(),
                newname.to_bytes(),
                RenameFlags::from_bits_retain(flags),
            )
            .guest()
    }

    fn link(
        &self,
        _ctx: Context,
        inode: Inode,
        newparent: Inode,
        newname: &CStr,
    ) -> io::Result<Entry> {
        let attr = self
            .fs
            .link(inode.into(), newparent.into(), newname.to_bytes())
            .guest()?;
        Ok(entry(&attr))
    }

    fn open(
        &self,
        _ctx: Context,
        inode: Inode,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        // FUSE_ATOMIC_O_TRUNC is always negotiated
        if flags & bindings::LINUX_O_TRUNC as u32 != 0 {
            self.truncate(inode, kill_priv)?;
        }
        // Only the guest changes files, so its page cache stays valid.
        Ok((None, OpenOptions::KEEP_CACHE))
    }

    fn create(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        _kill_priv: bool,
        _flags: u32,
        umask: u32,
        _extensions: Extensions,
    ) -> io::Result<(Entry, Option<Handle>, OpenOptions)> {
        let perm = ((mode & !umask) & 0o7777) as u16;
        let attr = self
            .fs
            .create(parent.into(), name.to_bytes(), perm, owner(&ctx))
            .guest()?;
        Ok((entry(&attr), None, OpenOptions::KEEP_CACHE))
    }

    fn read<W: io::Write + ZeroCopyWriter>(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Handle,
        mut w: W,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let mut buf = vec![0; size as usize];
        let count = self.fs.read(inode.into(), offset, &mut buf).guest()?;
        w.write_all(&buf[..count])?;
        Ok(count)
    }

    fn write<R: io::Read + ZeroCopyReader>(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Handle,
        mut r: R,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        kill_priv: bool,
        _flags: u32,
    ) -> io::Result<usize> {
        let mut buf = vec![0; size as usize];
        r.read_exact(&mut buf)?;
        let count = self.fs.write(inode.into(), offset, &buf).guest()?;
        if kill_priv {
            self.fs
                .setattr(
                    inode.into(),
                    &SetAttr {
                        kill_suid_sgid: true,
                        ..Default::default()
                    },
                )
                .guest()?;
        }
        Ok(count)
    }

    fn flush(
        &self,
        _ctx: Context,
        _inode: Inode,
        _handle: Handle,
        _lock_owner: u64,
    ) -> io::Result<()> {
        Ok(())
    }

    fn fsync(
        &self,
        _ctx: Context,
        inode: Inode,
        _datasync: bool,
        _handle: Handle,
    ) -> io::Result<()> {
        self.fs.fsync(inode.into()).guest()
    }

    fn fallocate(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Handle,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        // Only the default mode, 0, is supported.
        if mode != 0 {
            return Err(errno(Errno::ENOTSUP));
        }
        self.fs.allocate(inode.into(), offset, length).guest()
    }

    fn release(
        &self,
        _ctx: Context,
        _inode: Inode,
        _flags: u32,
        _handle: Handle,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        Ok(())
    }

    fn statfs(&self, _ctx: Context, _inode: Inode) -> io::Result<bindings::statvfs64> {
        let statfs = self.fs.statfs().guest()?;
        // SAFETY: statvfs is plain data; all-zero is a valid value.
        let mut st: bindings::statvfs64 = unsafe { mem::zeroed() };
        st.f_bsize = statfs.block_size as _;
        st.f_frsize = statfs.block_size as _;
        st.f_blocks = libc::fsblkcnt_t::try_from(statfs.blocks).unwrap_or(libc::fsblkcnt_t::MAX);
        st.f_bfree =
            libc::fsblkcnt_t::try_from(statfs.blocks_free).unwrap_or(libc::fsblkcnt_t::MAX);
        st.f_bavail = st.f_bfree;
        st.f_namemax = statfs.name_max as _;
        Ok(st)
    }

    fn opendir(
        &self,
        _ctx: Context,
        inode: Inode,
        _flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        let handle = self.fs.opendir(inode.into()).guest()?;
        Ok((Some(handle.into()), OpenOptions::empty()))
    }

    fn readdir<F>(
        &self,
        _ctx: Context,
        _inode: Inode,
        handle: Handle,
        _size: u32,
        offset: u64,
        mut add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        self.list(handle, offset, |offset, entry| {
            add_entry(DirEntry {
                ino: entry.inode.into(),
                offset,
                type_: entry.file_type.dirent_type().into(),
                name: &entry.name,
            })
        })
    }

    fn readdirplus<F>(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        _size: u32,
        offset: u64,
        mut add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry, Entry) -> io::Result<usize>,
    {
        self.list(handle, offset, |offset, dir_entry| {
            // The kernel takes no lookup reference for "." and ".."
            let attr = if is_dot(&dir_entry.name) {
                self.fs.getattr(dir_entry.inode)
            } else {
                self.fs.lookup(inode.into(), &dir_entry.name)
            }
            .guest()?;

            let added = add_entry(
                DirEntry {
                    ino: dir_entry.inode.into(),
                    offset,
                    type_: dir_entry.file_type.dirent_type().into(),
                    name: &dir_entry.name,
                },
                entry(&attr),
            )?;
            if added == 0 && !is_dot(&dir_entry.name) {
                // Did not fit: the kernel never saw this lookup
                self.fs.forget(attr.inode, 1);
            }
            Ok(added)
        })
    }

    fn fsyncdir(
        &self,
        _ctx: Context,
        inode: Inode,
        _datasync: bool,
        _handle: Handle,
    ) -> io::Result<()> {
        self.fs.fsync(inode.into()).guest()
    }

    fn releasedir(
        &self,
        _ctx: Context,
        _inode: Inode,
        _flags: u32,
        handle: Handle,
    ) -> io::Result<()> {
        self.fs.releasedir(DirHandle(handle));
        Ok(())
    }
    fn setupmapping(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Handle,
        foffset: u64,
        len: u64,
        flags: u64,
        moffset: u64,
        _shm_base: u64,
        _shm_size: u64,
        #[cfg(target_os = "macos")] _map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        let writable = flags & SetupmappingFlags::WRITE.bits() != 0;
        self.fs
            .setup_mapping(inode.into(), foffset, len, moffset, writable)
            .guest()
    }

    fn removemapping(
        &self,
        _ctx: Context,
        requests: Vec<RemovemappingOne>,
        _shm_base: u64,
        _shm_size: u64,
        #[cfg(target_os = "macos")] _map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        for request in requests {
            self.fs
                .remove_mapping(request.moffset, request.len)
                .guest()?;
        }
        Ok(())
    }
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
            window.fault(offset, access).is_ok()
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
