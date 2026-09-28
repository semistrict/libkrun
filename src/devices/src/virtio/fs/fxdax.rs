// virtio-fs backend serving an fxdax (Fxfs) image.
//
// All filesystem semantics live in `fxdax::Vfs`, which runs Fxfs on the
// in-process kernel; this adapter translates between it and the FUSE server.
// DAX mapping is not served yet: setupmapping and removemapping fail with
// ENOSYS.

use std::ffi::CStr;
use std::io;
use std::mem;
use std::path::Path;
use std::time::Duration;

use fxdax::vfs::{Attr, Creds, DirEntry as VfsDirEntry, FileKind, SetAttr, SetTime, Timespec};
use fxdax::{Result as VfsResult, Vfs};

use super::bindings;
use super::filesystem::{
    Context, DirEntry, Entry, Extensions, FileSystem, FsOptions, OpenOptions, SetattrValid,
    ZeroCopyReader, ZeroCopyWriter,
};

type Inode = u64;
type Handle = u64;

/// The guest is the only client of the image, so nothing changes behind its
/// back and cached names and attributes stay valid.
const TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

// Linux `d_type` values.
const DT_FIFO: u32 = 1;
const DT_CHR: u32 = 2;
const DT_DIR: u32 = 4;
const DT_BLK: u32 = 6;
const DT_REG: u32 = 8;
const DT_LNK: u32 = 10;
const DT_SOCK: u32 = 12;

/// Lowest inode number `AugmentFs` may hand to virtual entries: fxdax inode
/// numbers are Fxfs object ids, which an unencrypted volume allocates
/// sequentially from small numbers.
pub const FIRST_VIRTUAL_INODE: u64 = 1 << 63;

pub struct FxdaxFs {
    vfs: Vfs,
}

/// Conversion of `Vfs` results for the guest.
trait Guest<T> {
    fn guest(self) -> io::Result<T>;
}

impl<T> Guest<T> for VfsResult<T> {
    fn guest(self) -> io::Result<T> {
        // Vfs errors already carry Linux errno values.
        self.map_err(|err| io::Error::from_raw_os_error(err.errno()))
    }
}

fn creds(ctx: &Context) -> Creds {
    Creds {
        uid: ctx.uid,
        gid: ctx.gid,
    }
}

fn stat(attr: &Attr) -> bindings::stat64 {
    // SAFETY: stat is plain data; all-zero is a valid value.
    let mut st: bindings::stat64 = unsafe { mem::zeroed() };
    st.st_ino = attr.ino as _;
    st.st_mode = attr.mode() as _;
    st.st_nlink = attr.nlink as _;
    st.st_uid = attr.uid;
    st.st_gid = attr.gid;
    st.st_rdev = attr.rdev as _;
    st.st_size = attr.size as _;
    st.st_blocks = attr.blocks as _;
    st.st_blksize = attr.blksize as _;
    st.st_atime = attr.atime.secs as _;
    st.st_atime_nsec = attr.atime.nanos as _;
    st.st_mtime = attr.mtime.secs as _;
    st.st_mtime_nsec = attr.mtime.nanos as _;
    st.st_ctime = attr.ctime.secs as _;
    st.st_ctime_nsec = attr.ctime.nanos as _;
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
        inode: attr.ino,
        generation: 0,
        attr: stat(attr),
        attr_flags: 0,
        attr_timeout: TIMEOUT,
        entry_timeout: TIMEOUT,
    }
}

fn dir_type(kind: FileKind) -> u32 {
    match kind {
        FileKind::File => DT_REG,
        FileKind::Directory => DT_DIR,
        FileKind::Symlink => DT_LNK,
        FileKind::Fifo => DT_FIFO,
        FileKind::CharDevice => DT_CHR,
        FileKind::BlockDevice => DT_BLK,
        FileKind::Socket => DT_SOCK,
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
        Some(SetTime::At(Timespec {
            secs: secs as u64,
            nanos: nanos as u32,
        }))
    } else {
        None
    }
}

fn is_dot(name: &[u8]) -> bool {
    name == b"." || name == b".."
}

impl FxdaxFs {
    /// Open the fxdax image at `image`.
    pub fn open(image: &Path) -> io::Result<Self> {
        let vfs = Vfs::mount_image(image).map_err(|err| {
            io::Error::other(format!("mounting fxdax image {}: {err:?}", image.display()))
        })?;
        Ok(Self { vfs })
    }

    fn truncate(&self, inode: Inode, kill_priv: bool) -> io::Result<()> {
        self.vfs
            .setattr(
                inode,
                SetAttr {
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
        F: FnMut(u64, &VfsDirEntry) -> io::Result<usize>,
    {
        let entries = self.vfs.readdir(handle, offset).guest()?;
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
        if let Err(err) = self.vfs.unmount() {
            error!("fxdax: failed to write back: {err:?}");
        }
    }

    fn lookup(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        let attr = self.vfs.lookup(parent, name.to_bytes()).guest()?;
        Ok(entry(&attr))
    }

    fn forget(&self, _ctx: Context, inode: Inode, count: u64) {
        self.vfs.forget(inode, count);
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Option<Handle>,
    ) -> io::Result<(bindings::stat64, Duration)> {
        let attr = self.vfs.getattr(inode).guest()?;
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
        let attr = self.vfs.setattr(inode, set).guest()?;
        Ok((stat(&attr), TIMEOUT))
    }

    fn readlink(&self, _ctx: Context, inode: Inode) -> io::Result<Vec<u8>> {
        self.vfs.readlink(inode).guest()
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
            .vfs
            .symlink(parent, name.to_bytes(), linkname.to_bytes())
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
        let attr = self
            .vfs
            .mknod(creds(&ctx), parent, name.to_bytes(), mode & !umask, rdev)
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
            .vfs
            .mkdir(creds(&ctx), parent, name.to_bytes(), perm)
            .guest()?;
        Ok(entry(&attr))
    }

    fn unlink(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.vfs.unlink(parent, name.to_bytes()).guest()
    }

    fn rmdir(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.vfs.rmdir(parent, name.to_bytes()).guest()
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
        self.vfs
            .rename(
                olddir,
                oldname.to_bytes(),
                newdir,
                newname.to_bytes(),
                flags,
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
            .vfs
            .link(inode, newparent, newname.to_bytes())
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
            .vfs
            .create(creds(&ctx), parent, name.to_bytes(), perm)
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
        let count = self.vfs.read(inode, offset, &mut buf).guest()?;
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
        let count = self.vfs.write(inode, offset, &buf).guest()?;
        if kill_priv {
            self.vfs
                .setattr(
                    inode,
                    SetAttr {
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
        self.vfs.fsync(inode).guest()
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
        self.vfs.fallocate(inode, mode, offset, length).guest()
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
        let statfs = self.vfs.statfs().guest()?;
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
        let handle = self.vfs.opendir(inode).guest()?;
        Ok((Some(handle), OpenOptions::empty()))
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
                ino: entry.ino as _,
                offset,
                type_: dir_type(entry.kind),
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
                self.vfs.getattr(dir_entry.ino)
            } else {
                self.vfs.lookup(inode, &dir_entry.name)
            }
            .guest()?;

            let added = add_entry(
                DirEntry {
                    ino: dir_entry.ino as _,
                    offset,
                    type_: dir_type(dir_entry.kind),
                    name: &dir_entry.name,
                },
                entry(&attr),
            )?;
            if added == 0 && !is_dot(&dir_entry.name) {
                // Did not fit: the kernel never saw this lookup
                self.vfs.forget(attr.ino, 1);
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
        self.vfs.fsync(inode).guest()
    }

    fn releasedir(
        &self,
        _ctx: Context,
        _inode: Inode,
        _flags: u32,
        handle: Handle,
    ) -> io::Result<()> {
        self.vfs.releasedir(handle);
        Ok(())
    }
}
