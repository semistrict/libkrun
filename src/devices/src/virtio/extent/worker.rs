// The extent-store device's worker: a thread that takes the requests off the queue, and threads
// that serve them, blocking on the store's I/O, several at once: the device applies requests in any
// order but across barriers (doc-4), so a guest's MAP doesn't wait for the writes and durable
// commits queued before it.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use fxdax::{Errno, RemoteExtentStore, VolatileSlice, VolatileSliceMut};
use utils::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use utils::eventfd::EventFd;
use vm_memory::{ByteValued, GuestMemoryMmap};

use super::super::DeviceQueue;
use super::{
    CommitArgs, Info, IoArgs, MAX_ROOT, MAX_TRANSFER, MapArgs, RequestHeader, Response, RootLen,
    UnmapArgs, VIRTIO_EXTENT_COMMIT_DURABLE, VIRTIO_EXTENT_COMMIT_ROOT, VIRTIO_EXTENT_MAP_WRITE,
    VIRTIO_EXTENT_T_COMMIT,
    VIRTIO_EXTENT_T_FREE, VIRTIO_EXTENT_T_INFO, VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_T_READ,
    VIRTIO_EXTENT_T_ROOT, VIRTIO_EXTENT_T_UNMAP, VIRTIO_EXTENT_T_WRITE,
    VIRTIO_EXTENT_WRITE_BARRIER,
};
use crate::virtio::descriptor_utils::{Reader, Writer};
use crate::virtio::{DescriptorChain, InterruptTransport};

/// How many requests the worker serves at once: as many as the daemon does for a connection.
const SERVERS: usize = 8;

pub struct ExtentWorker {
    queue: DeviceQueue,
    interrupt: InterruptTransport,
    mem: GuestMemoryMmap,
    store: Arc<RemoteExtentStore>,
    stop_fd: EventFd,
}

impl ExtentWorker {
    pub fn new(
        queue: DeviceQueue,
        interrupt: InterruptTransport,
        mem: GuestMemoryMmap,
        store: Arc<RemoteExtentStore>,
        stop_fd: EventFd,
    ) -> Self {
        Self {
            queue,
            interrupt,
            mem,
            store,
            stop_fd,
        }
    }

    /// Spawns the worker's thread, which returns its queue once stopped.
    pub fn run(self) -> thread::JoinHandle<DeviceQueue> {
        thread::Builder::new()
            .name("extent worker".into())
            .spawn(|| self.work())
            .unwrap()
    }

    fn work(self) -> DeviceQueue {
        let Self {
            queue,
            interrupt,
            mem,
            store,
            stop_fd,
        } = self;
        let queue = Mutex::new(queue);
        let (requests, received) = mpsc::channel::<DescriptorChain>();
        let received = Mutex::new(received);
        thread::scope(|scope| {
            for server in 0..SERVERS {
                let (queue, interrupt, mem, store, received) =
                    (&queue, &interrupt, &mem, &store, &received);
                thread::Builder::new()
                    .name(format!("extent server {server}"))
                    .spawn_scoped(scope, move || loop {
                        let Ok(chain) = received.lock().unwrap().recv() else {
                            return;
                        };
                        let index = chain.index;
                        let len = handle(store, mem, chain);
                        complete(&mut queue.lock().unwrap(), interrupt, mem, index, len);
                    })
                    .unwrap();
            }
            // Once stopped, the servers finish what they took and return as the channel closes.
            take_requests(&queue, &mem, &stop_fd, requests);
        });
        queue.into_inner().unwrap()
    }
}

/// Takes the requests off `queue` as the driver notifies it, and sends them to `requests`, until
/// `stop_fd` is written.
fn take_requests<'a>(
    queue: &Mutex<DeviceQueue>,
    mem: &'a GuestMemoryMmap,
    stop_fd: &EventFd,
    requests: mpsc::Sender<DescriptorChain<'a>>,
) {
    let queue_fd = queue.lock().unwrap().event.as_raw_fd();
    let stop_raw_fd = stop_fd.as_raw_fd();
    let mut epoll = Epoll::new().unwrap();
    for fd in [queue_fd, stop_raw_fd] {
        epoll
            .ctl(
                ControlOperation::Add,
                fd,
                &EpollEvent::new(EventSet::IN, fd as u64),
            )
            .unwrap();
    }
    let mut events = vec![EpollEvent::new(EventSet::empty(), 0); 2];
    loop {
        let count = match epoll.wait(events.len(), -1, &mut events) {
            Ok(count) => count,
            Err(err) => {
                debug!("extent worker: epoll: {err}");
                continue;
            }
        };
        for event in &events[..count] {
            if event.fd() == stop_raw_fd {
                let _ = stop_fd.read();
                return;
            }
            if event.fd() == queue_fd {
                let mut queue = queue.lock().unwrap();
                if let Err(err) = queue.event.read() {
                    error!("extent worker: queue event: {err}");
                    continue;
                }
                loop {
                    queue.queue.disable_notification(mem).unwrap();
                    while let Some(chain) = queue.queue.pop(mem) {
                        // The servers outlive this sender.
                        requests.send(chain).unwrap();
                    }
                    if !queue.queue.enable_notification(mem).unwrap() {
                        break;
                    }
                }
            }
        }
    }
}

/// Returns the request at `index` to the driver, with `len` bytes written to it.
fn complete(
    queue: &mut DeviceQueue,
    interrupt: &InterruptTransport,
    mem: &GuestMemoryMmap,
    index: u16,
    len: u32,
) {
    if let Err(err) = queue.queue.add_used(mem, index, len) {
        error!("extent worker: adding a used element: {err:?}");
    }
    if queue.queue.needs_notification(mem).unwrap()
        && let Err(err) = interrupt.try_signal_used_queue()
    {
        error!("extent worker: signalling the queue: {err:?}");
    }
}

/// Serves the request in `chain` with `store`, and returns the length of what
/// the device writes to it: its writable buffers, whose last bytes hold the
/// response.
pub fn handle(store: &RemoteExtentStore, mem: &GuestMemoryMmap, chain: DescriptorChain) -> u32 {
    let (mut reader, mut writer) = match (Reader::new(mem, chain.clone()), Writer::new(mem, chain))
    {
        (Ok(reader), Ok(writer)) => (reader, writer),
        (Err(err), _) | (_, Err(err)) => {
            error!("extent device: a descriptor chain is not guest memory: {err:?}");
            return 0;
        }
    };
    // The response is the chain's last bytes, after what the request returns.
    let response_len = size_of::<Response>();
    let Some(data_len) = writer.available_bytes().checked_sub(response_len) else {
        error!("extent device: a request has no room for its response");
        return 0;
    };
    let mut data = Vec::new();
    let status = match serve(store, &mut reader, data_len, &mut data) {
        Ok(()) => 0,
        Err(errno) => {
            data.clear();
            errno.raw() as u32
        }
    };
    data.resize(data_len, 0);
    let response = Response {
        status: status.to_le(),
        reserved: 0,
    };
    if let Err(err) = writer
        .write_all(&data)
        .and_then(|()| writer.write_obj(response))
    {
        error!("extent device: writing a response: {err}");
        return 0;
    }
    // The response is at the end of what the driver gave, all of which counts as written, as
    // drivers read it whole.
    (data_len + response_len) as u32
}

/// An errno for a request the guest got wrong.
fn invalid(_: io::Error) -> Errno {
    Errno::EINVAL
}

/// Serves the request `reader` holds, appending what it returns to `out`,
/// which the driver gave `room` bytes for.
fn serve(
    store: &RemoteExtentStore,
    reader: &mut Reader,
    room: usize,
    out: &mut Vec<u8>,
) -> Result<(), Errno> {
    let header: RequestHeader = reader.read_obj().map_err(invalid)?;
    let flags = u32::from_le(header.flags);
    match u32::from_le(header.type_) {
        VIRTIO_EXTENT_T_READ => {
            let args: IoArgs = reader.read_obj().map_err(invalid)?;
            let len = u32::from_le(args.len);
            if len > MAX_TRANSFER || room < len as usize {
                return Err(Errno::EINVAL);
            }
            out.resize(len as usize, 0);
            store.read(
                args.id.into(),
                u64::from_le(args.offset),
                &mut [VolatileSliceMut::from(&mut out[..])],
            )
        }
        VIRTIO_EXTENT_T_WRITE => {
            let args: IoArgs = reader.read_obj().map_err(invalid)?;
            let len = u32::from_le(args.len);
            if len > MAX_TRANSFER {
                return Err(Errno::EINVAL);
            }
            let mut data = vec![0; len as usize];
            reader.read_exact(&mut data).map_err(invalid)?;
            if flags & VIRTIO_EXTENT_WRITE_BARRIER != 0 {
                // A barrier, which orders, without waiting for durability (doc-4).
                store.commit_ordered(None)?;
            }
            store.write(
                args.id.into(),
                u64::from_le(args.offset),
                &[VolatileSlice::from(&data[..])],
            )
        }
        VIRTIO_EXTENT_T_FREE => {
            let id: super::ExtentIdLe = reader.read_obj().map_err(invalid)?;
            store.free(id.into())
        }
        VIRTIO_EXTENT_T_MAP => {
            let args: MapArgs = reader.read_obj().map_err(invalid)?;
            if flags & !VIRTIO_EXTENT_MAP_WRITE != 0 {
                return Err(Errno::EINVAL);
            }
            store.map(
                args.id.into(),
                u64::from_le(args.offset),
                u64::from_le(args.len),
                u64::from_le(args.window_offset),
                flags & VIRTIO_EXTENT_MAP_WRITE != 0,
            )
        }
        VIRTIO_EXTENT_T_UNMAP => {
            let args: UnmapArgs = reader.read_obj().map_err(invalid)?;
            store.unmap(u64::from_le(args.window_offset), u64::from_le(args.len))
        }
        VIRTIO_EXTENT_T_COMMIT => {
            let args: CommitArgs = reader.read_obj().map_err(invalid)?;
            let mut root = None;
            if flags & VIRTIO_EXTENT_COMMIT_ROOT != 0 {
                let len = u32::from_le(args.root_len);
                if len > MAX_ROOT {
                    return Err(Errno::EINVAL);
                }
                let mut data = vec![0; len as usize];
                reader.read_exact(&mut data).map_err(invalid)?;
                root = Some(data);
            }
            // Without COMMIT_DURABLE, the commit completes once ordered: the daemon makes it
            // durable soon after (doc-4).
            if flags & VIRTIO_EXTENT_COMMIT_DURABLE != 0 {
                store.commit(root.as_deref())
            } else {
                store.commit_ordered(root.as_deref())
            }
        }
        VIRTIO_EXTENT_T_ROOT => {
            let root = store.root()?;
            if room < MAX_ROOT as usize + size_of::<RootLen>() || root.len() > MAX_ROOT as usize {
                return Err(Errno::EINVAL);
            }
            out.extend_from_slice(&root);
            out.resize(MAX_ROOT as usize, 0);
            let len = RootLen {
                len: (root.len() as u32).to_le(),
                reserved: 0,
            };
            out.extend_from_slice(len.as_slice());
            Ok(())
        }
        VIRTIO_EXTENT_T_INFO => {
            if room < size_of::<Info>() {
                return Err(Errno::EINVAL);
            }
            let info = store.info()?;
            let info = Info {
                capacity: info.capacity.to_le(),
                used: info.used.to_le(),
                available: info.available.to_le(),
                next_sequence: info.next_sequence.to_le(),
            };
            out.extend_from_slice(info.as_slice());
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
}

#[cfg(test)]
mod tests {
    use fxdax::ExtentStore;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use vm_memory::{Bytes, GuestAddress};

    use super::super::{ExtentIdLe, VIRTIO_EXTENT_COMMIT_DURABLE, VIRTIO_EXTENT_T_COMMIT};
    use super::*;
    use crate::virtio::descriptor_utils::{DescriptorType, create_descriptor_chain};

    const BLOCK: usize = ExtentStore::BLOCK_SIZE as usize;
    const BUFFERS: GuestAddress = GuestAddress(0x1000);

    /// A directory of the test's, removed once dropped.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "extent-device-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A store of 1 MiB, which an extent daemon of the test's serves.
    fn store() -> (RemoteExtentStore, TestDir) {
        let dir = TestDir::new();
        let socket = dir.0.join("extentd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        thread::spawn(move || fxdax::extentd::serve(listener));
        let store = RemoteExtentStore::connect(&socket, &dir.0.join("store"), 1 << 20).unwrap();
        (store, dir)
    }

    /// The bytes of `request`'s header and arguments, followed by `data`.
    fn bytes<T: ByteValued>(type_: u32, flags: u32, args: Option<T>, data: &[u8]) -> Vec<u8> {
        let header = RequestHeader {
            type_,
            flags,
            unique: 7,
        };
        let mut bytes = header.as_slice().to_vec();
        if let Some(args) = &args {
            bytes.extend_from_slice(args.as_slice());
        }
        bytes.extend_from_slice(data);
        bytes
    }

    /// Serves a request of `readable` bytes, with `writable` bytes for the
    /// device to write, the response included, and returns what the device
    /// wrote, the response apart, and the response's status.
    fn request(store: &RemoteExtentStore, readable: &[u8], writable: usize) -> (Vec<u8>, u32) {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        mem.write_slice(readable, BUFFERS).unwrap();
        let chain = create_descriptor_chain(
            &mem,
            GuestAddress(0),
            BUFFERS,
            vec![
                (DescriptorType::Readable, readable.len() as u32),
                (DescriptorType::Writable, writable as u32),
            ],
            0,
        )
        .unwrap();
        let used = handle(store, &mem, chain) as usize;
        let mut written = vec![0; writable];
        mem.read_slice(
            &mut written,
            GuestAddress(BUFFERS.0 + readable.len() as u64),
        )
        .unwrap();
        let response_at = written.len() - size_of::<Response>();
        let response = *Response::from_slice(&written[response_at..]).unwrap();
        assert_eq!(
            used, writable,
            "the device wrote {used} of {writable} bytes"
        );
        written.truncate(response_at);
        (written, response.status)
    }

    fn io(store: &RemoteExtentStore, sequence: u64, offset: u64, len: usize) -> IoArgs {
        IoArgs {
            id: ExtentIdLe {
                branch: store.branch(),
                sequence,
            },
            offset,
            len: len as u32,
            reserved: 0,
        }
    }

    const RESPONSE: usize = size_of::<Response>();

    #[test]
    fn writes_and_reads() {
        let (store, _dir) = store();
        let data: Vec<u8> = (0..2 * BLOCK).map(|i| i as u8).collect();
        let write = bytes(
            VIRTIO_EXTENT_T_WRITE,
            0,
            Some(io(&store, 1, 0, data.len())),
            &data,
        );
        assert_eq!(request(&store, &write, RESPONSE), (vec![], 0));
        let read = bytes(
            VIRTIO_EXTENT_T_READ,
            0,
            Some(io(&store, 1, 0, data.len())),
            &[],
        );
        assert_eq!(
            request(&store, &read, data.len() + RESPONSE),
            (data.clone(), 0)
        );

        // An extent the store doesn't hold, and a write after a commit sealed the extent.
        let missing = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&store, 2, 0, BLOCK)), &[]);
        assert_eq!(
            request(&store, &missing, BLOCK + RESPONSE).1,
            Errno::ENOENT.raw() as u32
        );
        let commit = bytes(
            VIRTIO_EXTENT_T_COMMIT,
            VIRTIO_EXTENT_COMMIT_DURABLE,
            Some(CommitArgs::default()),
            &[],
        );
        assert_eq!(request(&store, &commit, RESPONSE), (vec![], 0));
        let rewrite = bytes(
            VIRTIO_EXTENT_T_WRITE,
            0,
            Some(io(&store, 1, 0, BLOCK)),
            &data[..BLOCK],
        );
        assert_eq!(
            request(&store, &rewrite, RESPONSE).1,
            Errno::EPERM.raw() as u32
        );
        // A barrier commits what came before it.
        let barrier = bytes(
            VIRTIO_EXTENT_T_WRITE,
            VIRTIO_EXTENT_WRITE_BARRIER,
            Some(io(&store, 3, 0, BLOCK)),
            &data[..BLOCK],
        );
        assert_eq!(request(&store, &barrier, RESPONSE).1, 0);

        let free = bytes(
            VIRTIO_EXTENT_T_FREE,
            0,
            Some(ExtentIdLe {
                branch: store.branch(),
                sequence: 1,
            }),
            &[],
        );
        assert_eq!(request(&store, &free, RESPONSE), (vec![], 0));
        assert_eq!(
            request(&store, &read, data.len() + RESPONSE).1,
            Errno::ENOENT.raw() as u32
        );
    }

    #[test]
    fn roots_and_info() {
        let (store, _dir) = store();
        let root = bytes::<RootLen>(VIRTIO_EXTENT_T_ROOT, 0, None, &[]);
        let root_len = MAX_ROOT as usize + size_of::<RootLen>();
        let (written, status) = request(&store, &root, root_len + RESPONSE);
        assert_eq!(status, 0);
        assert_eq!(
            RootLen::from_slice(&written[MAX_ROOT as usize..])
                .unwrap()
                .len,
            0
        );

        let commit = bytes(
            VIRTIO_EXTENT_T_COMMIT,
            VIRTIO_EXTENT_COMMIT_ROOT,
            Some(CommitArgs {
                root_len: 5,
                reserved: 0,
            }),
            b"first",
        );
        assert_eq!(request(&store, &commit, RESPONSE), (vec![], 0));
        let (written, status) = request(&store, &root, root_len + RESPONSE);
        assert_eq!(status, 0);
        assert_eq!(&written[..5], b"first");
        assert_eq!(
            RootLen::from_slice(&written[MAX_ROOT as usize..])
                .unwrap()
                .len,
            5
        );

        let info = bytes::<Info>(VIRTIO_EXTENT_T_INFO, 0, None, &[]);
        let (written, status) = request(&store, &info, size_of::<Info>() + RESPONSE);
        assert_eq!(status, 0);
        assert_eq!(
            *Info::from_slice(&written).unwrap(),
            Info {
                capacity: 1 << 20,
                used: 0,
                available: 1 << 20,
                next_sequence: 0
            }
        );
    }

    #[test]
    fn refuses_bad_requests() {
        let (store, _dir) = store();
        let unknown = bytes::<Info>(99, 0, None, &[]);
        assert_eq!(
            request(&store, &unknown, RESPONSE).1,
            Errno::EINVAL.raw() as u32
        );
        // A read with less room than it reads, and one whose header is short.
        let read = bytes(
            VIRTIO_EXTENT_T_READ,
            0,
            Some(io(&store, 1, 0, 2 * BLOCK)),
            &[],
        );
        assert_eq!(
            request(&store, &read, BLOCK + RESPONSE).1,
            Errno::EINVAL.raw() as u32
        );
        assert_eq!(
            request(&store, &[0; 4], RESPONSE).1,
            Errno::EINVAL.raw() as u32
        );
        // Without a DAX window, writable or not; and a MAP flag the device doesn't know.
        for flags in [0, VIRTIO_EXTENT_MAP_WRITE] {
            let map = bytes(VIRTIO_EXTENT_T_MAP, flags, Some(MapArgs::default()), &[]);
            assert_eq!(
                request(&store, &map, RESPONSE).1,
                Errno::ENOSYS.raw() as u32
            );
        }
        let map = bytes(VIRTIO_EXTENT_T_MAP, 1 << 1, Some(MapArgs::default()), &[]);
        assert_eq!(
            request(&store, &map, RESPONSE).1,
            Errno::EINVAL.raw() as u32
        );
    }
}
