#[cfg(target_os = "macos")]
use crossbeam_channel::Sender;
#[cfg(target_os = "macos")]
use utils::worker_message::WorkerMessage;

use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::thread;

use utils::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use utils::eventfd::EventFd;
use vm_memory::GuestMemoryMmap;

use super::super::{FsError, Queue};
use super::augment_fs::AugmentFs;
use super::defs::{HPQ_INDEX, REQ_INDEX};
use super::descriptor_utils::{Reader, Writer};
use super::device::FsBackend;
#[cfg(feature = "fxdax")]
use super::fxdax::FxdaxServer;
use super::inode_alloc::InodeAllocator;
use super::null_fs::NullFs;
use super::passthrough::PassthroughFs;
#[cfg(target_os = "macos")]
use super::passthrough::PassthroughFsSnapshot;
use super::read_only::PassthroughFsRo;
use super::server::Server;
use super::virtual_entry::VirtualDirEntry;
use crate::virtio::queue::DescriptorChain;
use crate::virtio::{InterruptTransport, VirtioShmRegion};

pub(crate) enum FsServer {
    ReadWrite(Server<AugmentFs<PassthroughFs>>),
    ReadOnly(Server<AugmentFs<PassthroughFsRo>>),
    Null(Server<AugmentFs<NullFs>>),
    #[cfg(feature = "fxdax")]
    Fxdax(FxdaxServer),
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum FsServerSnapshot {
    ReadWrite {
        options_bits: u64,
        passthrough: PassthroughFsSnapshot,
    },
    ReadOnly {
        options_bits: u64,
        passthrough: PassthroughFsSnapshot,
    },
}

impl FsServer {
    #[cfg(target_os = "macos")]
    pub(crate) fn snapshot_state(&self) -> io::Result<FsServerSnapshot> {
        match self {
            FsServer::ReadWrite(s) => Ok(FsServerSnapshot::ReadWrite {
                options_bits: s.options_bits(),
                passthrough: s.fs().inner().snapshot_state()?,
            }),
            FsServer::ReadOnly(s) => Ok(FsServerSnapshot::ReadOnly {
                options_bits: s.options_bits(),
                passthrough: s.fs().inner().snapshot_state()?,
            }),
            FsServer::Null(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "virtio-fs null server snapshot is unsupported",
            )),
            #[cfg(feature = "fxdax")]
            FsServer::Fxdax(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "virtio-fs fxdax server snapshot is unsupported",
            )),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn restore_state(&self, snap: &FsServerSnapshot) -> io::Result<()> {
        match (self, snap) {
            (
                FsServer::ReadWrite(s),
                FsServerSnapshot::ReadWrite {
                    options_bits,
                    passthrough,
                },
            ) => {
                s.fs().inner().restore_state(passthrough)?;
                s.restore_options_bits(*options_bits);
                Ok(())
            }
            (
                FsServer::ReadOnly(s),
                FsServerSnapshot::ReadOnly {
                    options_bits,
                    passthrough,
                },
            ) => {
                s.fs().inner().restore_state(passthrough)?;
                s.restore_options_bits(*options_bits);
                Ok(())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "virtio-fs server mode mismatch",
            )),
        }
    }

    /// Handles the request in `head`, and returns the length of its reply.
    fn handle_chain(
        &self,
        mem: &GuestMemoryMmap,
        head: DescriptorChain,
        shm_region: &Option<VirtioShmRegion>,
        exit_code: &Arc<AtomicI32>,
        #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
    ) -> usize {
        let r = Reader::new(mem, head.clone())
            .map_err(FsError::QueueReader)
            .unwrap();
        let w = Writer::new(mem, head)
            .map_err(FsError::QueueWriter)
            .unwrap();
        let result = match self {
            FsServer::ReadWrite(s) => s.handle_message(
                r,
                w,
                shm_region,
                exit_code,
                #[cfg(target_os = "macos")]
                map_sender,
            ),
            FsServer::ReadOnly(s) => s.handle_message(
                r,
                w,
                shm_region,
                exit_code,
                #[cfg(target_os = "macos")]
                map_sender,
            ),
            FsServer::Null(s) => s.handle_message(
                r,
                w,
                shm_region,
                exit_code,
                #[cfg(target_os = "macos")]
                map_sender,
            ),
            #[cfg(feature = "fxdax")]
            FsServer::Fxdax(_) => unreachable!("fxdax serves its queues itself"),
        };
        result.unwrap_or_else(|e| {
            error!("error handling message: {e:?}");
            0
        })
    }
}

pub struct FsWorker {
    queues: Vec<Queue>,
    queue_evts: Vec<Arc<EventFd>>,
    interrupt: InterruptTransport,
    mem: GuestMemoryMmap,
    shm_region: Option<VirtioShmRegion>,
    server: FsServer,
    stop_fd: EventFd,
    exit_code: Arc<AtomicI32>,
    #[cfg(target_os = "macos")]
    map_sender: Option<Sender<WorkerMessage>>,
}

pub struct FsWorkerStopResult {
    pub queues: Vec<Queue>,
    pub queue_evts: Vec<Arc<EventFd>>,
    pub server: FsServer,
}

impl FsWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        queues: Vec<Queue>,
        queue_evts: Vec<Arc<EventFd>>,
        interrupt: InterruptTransport,
        mem: GuestMemoryMmap,
        shm_region: Option<VirtioShmRegion>,
        backend: FsBackend,
        read_only: bool,
        virtual_entries: Vec<VirtualDirEntry>,
        stop_fd: EventFd,
        exit_code: Arc<AtomicI32>,
        #[cfg(target_os = "macos")] map_sender: Option<Sender<WorkerMessage>>,
        #[cfg(all(feature = "fxdax", target_os = "linux"))] prefault: Option<
            Arc<dyn super::PrefaultGuestMemory>,
        >,
    ) -> Result<Self, io::Error> {
        let inode_alloc = Arc::new(InodeAllocator::new());
        let server = match backend {
            FsBackend::Passthrough(cfg) if read_only => {
                let inner = PassthroughFsRo::new(cfg, inode_alloc.clone())?;
                FsServer::ReadOnly(Server::new(AugmentFs::new(
                    inner,
                    &inode_alloc,
                    virtual_entries,
                )))
            }
            FsBackend::Passthrough(cfg) => {
                let inner = PassthroughFs::new(cfg, inode_alloc.clone())?;
                FsServer::ReadWrite(Server::new(AugmentFs::new(
                    inner,
                    &inode_alloc,
                    virtual_entries,
                )))
            }
            FsBackend::Null => FsServer::Null(Server::new(AugmentFs::new(
                NullFs,
                &inode_alloc,
                virtual_entries,
            ))),
            #[cfg(feature = "fxdax")]
            FsBackend::Fxdax { image, volume } => {
                if read_only {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "read-only fxdax devices are unsupported",
                    ));
                }
                // fxdax serves FUSE itself, so libkrun cannot add entries,
                // such as a root's implicit init.
                if !virtual_entries.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "fxdax devices cannot have virtual entries, such as a root's implicit init",
                    ));
                }
                FsServer::Fxdax(FxdaxServer::open(
                    &image,
                    volume.as_deref(),
                    shm_region.as_ref(),
                    #[cfg(target_os = "linux")]
                    prefault,
                )?)
            }
        };
        Ok(Self {
            queues,
            queue_evts,
            interrupt,
            mem,
            shm_region,
            server,
            stop_fd,
            exit_code,
            #[cfg(target_os = "macos")]
            map_sender,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        queues: Vec<Queue>,
        queue_evts: Vec<Arc<EventFd>>,
        interrupt: InterruptTransport,
        mem: GuestMemoryMmap,
        shm_region: Option<VirtioShmRegion>,
        server: FsServer,
        stop_fd: EventFd,
        exit_code: Arc<AtomicI32>,
        #[cfg(target_os = "macos")] map_sender: Option<Sender<WorkerMessage>>,
    ) -> Self {
        Self {
            queues,
            queue_evts,
            interrupt,
            mem,
            shm_region,
            server,
            stop_fd,
            exit_code,
            #[cfg(target_os = "macos")]
            map_sender,
        }
    }

    pub fn run(self) -> thread::JoinHandle<FsWorkerStopResult> {
        #[cfg(feature = "fxdax")]
        if let FsServer::Fxdax(server) = &self.server {
            // The loop runs on the filesystem's threads, and this thread only
            // waits for it to stop.
            let spawner = server.spawner();
            let (stopped, stop) = std::sync::mpsc::sync_channel(1);
            spawner.spawn(async move {
                let _ = stopped.send(self.serve_fxdax().await);
            });
            return thread::Builder::new()
                .name("fs worker".into())
                .spawn(move || stop.recv().expect("the fxdax device's loop panicked"))
                .unwrap();
        }
        thread::Builder::new()
            .name("fs worker".into())
            .spawn(|| self.work())
            .unwrap()
    }

    fn work(mut self) -> FsWorkerStopResult {
        let virtq_hpq_ev_fd = self.queue_evts[HPQ_INDEX].as_raw_fd();
        let virtq_req_ev_fd = self.queue_evts[REQ_INDEX].as_raw_fd();
        let stop_ev_fd = self.stop_fd.as_raw_fd();

        let mut epoll = Epoll::new().unwrap();

        let _ = epoll.ctl(
            ControlOperation::Add,
            virtq_hpq_ev_fd,
            &EpollEvent::new(EventSet::IN, virtq_hpq_ev_fd as u64),
        );
        let _ = epoll.ctl(
            ControlOperation::Add,
            virtq_req_ev_fd,
            &EpollEvent::new(EventSet::IN, virtq_req_ev_fd as u64),
        );
        let _ = epoll.ctl(
            ControlOperation::Add,
            stop_ev_fd,
            &EpollEvent::new(EventSet::IN, stop_ev_fd as u64),
        );

        let mut epoll_events = vec![EpollEvent::new(EventSet::empty(), 0); 32];
        loop {
            match epoll.wait(epoll_events.len(), -1, epoll_events.as_mut_slice()) {
                Ok(ev_cnt) => {
                    for event in &epoll_events[0..ev_cnt] {
                        let source = event.fd();
                        let event_set = event.event_set();
                        match event_set {
                            EventSet::IN if source == virtq_hpq_ev_fd => {
                                self.handle_event(HPQ_INDEX);
                            }
                            EventSet::IN if source == virtq_req_ev_fd => {
                                self.handle_event(REQ_INDEX);
                            }
                            EventSet::IN if source == stop_ev_fd => {
                                debug!("stopping worker thread");
                                let _ = self.stop_fd.read();
                                return FsWorkerStopResult {
                                    queues: self.queues,
                                    queue_evts: self.queue_evts,
                                    server: self.server,
                                };
                            }
                            _ => {
                                log::warn!(
                                    "Received unknown event: {event_set:?} from fd: {source:?}"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!("failed to consume muxer epoll event: {e}");
                }
            }
        }
    }

    /// Serves the queues on fxdax's own threads, until the stop event: a
    /// queue's event wakes the thread that also runs Fxfs's work for the
    /// requests, which it handles concurrently, completing each as it finishes.
    #[cfg(feature = "fxdax")]
    async fn serve_fxdax(self) -> FsWorkerStopResult {
        use futures::stream::{FuturesUnordered, StreamExt};
        use futures::{FutureExt, select_biased};
        use tokio::io::unix::AsyncFd;

        let FsWorker {
            mut queues,
            queue_evts,
            interrupt,
            mem,
            server,
            stop_fd,
            ..
        } = self;
        let FsServer::Fxdax(fxdax) = &server else {
            unreachable!()
        };
        let events = [HPQ_INDEX, REQ_INDEX]
            .map(|index| AsyncFd::new(queue_evts[index].as_raw_fd()).expect("watch a queue"));
        let stop = AsyncFd::new(stop_fd.as_raw_fd()).expect("watch the stop event");
        let mut requests = FuturesUnordered::new();
        loop {
            // Take every request the queues have, as process_queue does.
            for index in [HPQ_INDEX, REQ_INDEX] {
                let queue = &mut queues[index];
                loop {
                    queue.disable_notification(&mem).unwrap();
                    while let Some(head) = queue.pop(&mem) {
                        let head_index = head.index;
                        let mem = &mem;
                        requests.push(
                            async move { (index, head_index, fxdax.handle(mem, head).await) },
                        );
                    }
                    if !queue.enable_notification(&mem).unwrap() {
                        break;
                    }
                }
            }
            select_biased! {
                (index, head_index, len) = requests.select_next_some() => {
                    let queue = &mut queues[index];
                    if let Err(e) = queue.add_used(&mem, head_index, len as u32) {
                        error!("failed to add used elements to the queue: {e:?}");
                    }
                    if queue.needs_notification(&mem).unwrap() {
                        interrupt.signal_used_queue();
                    }
                }
                guard = events[HPQ_INDEX].readable().fuse() => {
                    let _ = queue_evts[HPQ_INDEX].read();
                    guard.expect("a queue's event").clear_ready();
                }
                guard = events[REQ_INDEX].readable().fuse() => {
                    let _ = queue_evts[REQ_INDEX].read();
                    guard.expect("a queue's event").clear_ready();
                }
                guard = stop.readable().fuse() => {
                    debug!("stopping the fxdax device's loop");
                    let _ = stop_fd.read();
                    guard.expect("the stop event").clear_ready();
                    break;
                }
            }
        }
        // The requests in flight complete before the device stops.
        while let Some((index, head_index, len)) = requests.next().await {
            let queue = &mut queues[index];
            if let Err(e) = queue.add_used(&mem, head_index, len as u32) {
                error!("failed to add used elements to the queue: {e:?}");
            }
            if queue.needs_notification(&mem).unwrap() {
                interrupt.signal_used_queue();
            }
        }
        drop(requests);
        drop((events, stop));
        FsWorkerStopResult {
            queues,
            queue_evts,
            server,
        }
    }

    fn handle_event(&mut self, queue_index: usize) {
        debug!("Fs: queue event: {queue_index}");
        if let Err(e) = self.queue_evts[queue_index].read() {
            error!("Failed to get queue event: {e:?}");
        }

        loop {
            self.queues[queue_index]
                .disable_notification(&self.mem)
                .unwrap();

            self.process_queue(queue_index);

            if !self.queues[queue_index]
                .enable_notification(&self.mem)
                .unwrap()
            {
                break;
            }
        }
    }

    fn process_queue(&mut self, queue_index: usize) {
        let queue = &mut self.queues[queue_index];
        while let Some(head) = queue.pop(&self.mem) {
            let index = head.index;
            let len = self.server.handle_chain(
                &self.mem,
                head,
                &self.shm_region,
                &self.exit_code,
                #[cfg(target_os = "macos")]
                &self.map_sender,
            );

            if let Err(e) = queue.add_used(&self.mem, index, len as u32) {
                error!("failed to add used elements to the queue: {e:?}");
            }

            if queue.needs_notification(&self.mem).unwrap() {
                self.interrupt.signal_used_queue();
            }
        }
    }
}
