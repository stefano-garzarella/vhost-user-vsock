// Copyright 2020 Red Hat, Inc. All Rights Reserved.
//
// Portions Copyright 2019 Intel Corporation. All Rights Reserved.
//
// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)

extern crate log;
extern crate vhost_rs;
extern crate vhost_user_backend;
extern crate vm_virtio;

use epoll;
use libc::{self, EFD_NONBLOCK};
use log::*;
use std::fmt;
use std::io::{self};
use std::process;
use std::sync::{Arc, Mutex, RwLock};
use std::vec::Vec;
use std::slice;
use std::mem;
use vhost_rs::vhost_user::message::*;
use vhost_rs::vhost_user::Listener;
use vhost_rs::vhost_user::Error as VhostUserError;
use vhost_user_backend::{VhostUserBackend, VhostUserDaemon, Vring, VringWorker};
use virtio_bindings::bindings::virtio_net::*;
use virtio_bindings::bindings::virtio_ring::*;
use virtio_bindings::bindings::virtio_ring::__u64;
use vm_memory::GuestMemoryMmap;
use vm_virtio::{DeviceEventT};
use vmm::config::{OptionParser, OptionParserError};
use vmm_sys_util::eventfd::EventFd;
use vm_virtio::vsock::{VsockPacket, VsockChannel, VsockUnixBackend, VsockEpollListener};

const QUEUE_SIZE: usize = 128;
const NUM_QUEUES: usize = 2;

// New descriptors are pending on the rx queue.
pub const RX_QUEUE_EVENT: DeviceEventT = 0;
// New descriptors are pending on the tx queue.
pub const TX_QUEUE_EVENT: DeviceEventT = 1;
// New descriptors are pending on the event queue.
pub const EVT_QUEUE_EVENT: DeviceEventT = 2;
// Notification coming from the backend.
pub const BACKEND_EVENT: DeviceEventT = 3;

pub type VhostUserResult<T> = std::result::Result<T, VhostUserError>;
pub type Result<T> = std::result::Result<T, Error>;
pub type VhostUserBackendResult<T> = std::result::Result<T, std::io::Error>;

//TODO: move in the virtio bindings
#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct virtio_vsock_config {
    pub guest_cid: __u64,
}

#[derive(Debug)]
pub enum Error {
    /// Failed to create kill eventfd
    CreateKillEventFd(io::Error),
    /// Failed to parse configuration string
    FailedConfigParse(OptionParserError),
    /// Failed to handle event other than input event.
    HandleEventNotEpollIn,
    /// Failed to handle unknown event.
    HandleEventUnknownEvent,
    /// Cannot create virtio-vsock backend
    CreateVsockBackend(vm_virtio::vsock::VsockUnixError),
    /// No uds_path provided
    UDSPathParameterMissing,
    /// No guest_cid provided
    GuestCIDParameterMissing,
    /// No socket provided
    SocketParameterMissing,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vhost_user_vsock_error: {:?}", self)
    }
}

impl std::error::Error for Error {}

impl std::convert::From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        std::io::Error::new(io::ErrorKind::Other, e)
    }
}

struct VhostUserVsockThread {
    mem: Option<GuestMemoryMmap>,
    event_idx: bool,
    kill_evt: EventFd,
    unix_backend: VsockUnixBackend,
    vring_worker: Option<Arc<VringWorker>>,
}

impl VhostUserVsockThread {
    fn new(
        unix_backend: VsockUnixBackend,
    ) -> Result<Self> {
        Ok(VhostUserVsockThread {
            mem: None,
            event_idx: false,
            kill_evt: EventFd::new(EFD_NONBLOCK).map_err(Error::CreateKillEventFd)?,
            unix_backend,
            vring_worker: None,
        })
    }

    pub fn set_vring_worker(&mut self, vring_worker: Option<Arc<VringWorker>>) {
        self.vring_worker = vring_worker;

        self.vring_worker.as_ref().unwrap().register_listener(
            self.unix_backend.get_polled_fd(),
            self.unix_backend.get_polled_evset(),
            u64::from(BACKEND_EVENT),
        ).unwrap();
    }

    fn process_rx(&mut self, vring: &mut Vring) -> bool {
        debug!("vsock: epoll_handler::process_rx()");

        let mem = match self.mem.as_ref() {
            Some(m) => m,
            None => return false,
        };

        let mut used_desc_heads = [(0, 0); QUEUE_SIZE as usize];
        let mut used_count = 0;
        for avail_desc in vring.mut_queue().iter(&mem) {
            let used_len = match VsockPacket::from_rx_virtq_head(&avail_desc) {
                Ok(mut pkt) => {
                    if self.unix_backend.recv_pkt(&mut pkt).is_ok() {
                        pkt.hdr().len() as u32 + pkt.len()
                    } else {
                        // We are using a consuming iterator over the virtio buffers, so, if we can't
                        // fill in this buffer, we'll need to undo the last iterator step.
                        vring.mut_queue().go_to_previous_position();
                        break;
                    }
                }
                Err(e) => {
                    warn!("vsock: RX queue error: {:?}", e);
                    0
                }
            };

            used_desc_heads[used_count] = (avail_desc.index, used_len);
            used_count += 1;
        }

        for &(desc_index, len) in &used_desc_heads[..used_count] {
            vring.mut_queue().add_used(&mem, desc_index, len);
        }

        if used_count > 0 {
            debug!("signalling RX queue");
            vring.signal_used_queue().unwrap();
            return true;
        }

        false
    }

    fn process_tx(&mut self, vring: &mut Vring) -> bool {
        debug!("vsock: epoll_handler::process_tx()");

        let mem = match self.mem.as_ref() {
            Some(m) => m,
            None => return false,
        };

        let mut used_desc_heads = [(0, 0); QUEUE_SIZE as usize];
        let mut used_count = 0;
        for avail_desc in vring.mut_queue().iter(&mem) {
            let pkt = match VsockPacket::from_tx_virtq_head(&avail_desc) {
                Ok(pkt) => pkt,
                Err(e) => {
                    error!("vsock: error reading TX packet: {:?}", e);
                    used_desc_heads[used_count] = (avail_desc.index, 0);
                    used_count += 1;
                    continue;
                }
            };

            if self.unix_backend.send_pkt(&pkt).is_err() {
                vring.mut_queue().go_to_previous_position();
                break;
            }

            used_desc_heads[used_count] = (avail_desc.index, 0);
            used_count += 1;
        }

        for &(desc_index, len) in &used_desc_heads[..used_count] {
            vring.mut_queue().add_used(&mem, desc_index, len);
        }

        if used_count > 0 {
            debug!("signalling TX queue");
            vring.signal_used_queue().unwrap();
            return true;
        }

        false
    }
}
pub struct VhostUserVsockBackend {
    threads: Vec<Mutex<VhostUserVsockThread>>,
    config: virtio_vsock_config,
    queues_per_thread: Vec<u64>,
}

impl VhostUserVsockBackend {

    /// Create a new virtio vsock device with the given guest_cid
    pub fn new(
        guest_cid: u32,
        uds_path: String,
    ) -> Result<Self> {
        let config = virtio_vsock_config {
            guest_cid: guest_cid.into(),
        };

        let mut queues_per_thread = Vec::new();
        let mut threads = Vec::new();

        let unix_backend = VsockUnixBackend::new(
            guest_cid.into(),
            uds_path,
        ).map_err(Error::CreateVsockBackend)?;

        let thread = Mutex::new(VhostUserVsockThread::new(
            unix_backend,
        )?);
        threads.push(thread);
        queues_per_thread.push(0b11);

        Ok(VhostUserVsockBackend {
            config,
            threads,
            queues_per_thread,
        })
    }
}

impl VhostUserBackend for VhostUserVsockBackend {
    fn num_queues(&self) -> usize {
        NUM_QUEUES
    }

    fn queues_per_thread(&self) -> Vec<u64> {
        self.queues_per_thread.clone()
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE
    }

    fn features(&self) -> u64 {
        1 << VIRTIO_F_VERSION_1
            | 1 << VIRTIO_F_NOTIFY_ON_EMPTY
            | 1 << VIRTIO_RING_F_EVENT_IDX
            // TODO: | 1 << VIRTIO_RING_F_INDIRECT_DESC
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG
    }

    fn set_event_idx(&mut self, enabled: bool) {
        debug!("event_idx {:?}\n", enabled);
        for thread in self.threads.iter() {
            thread.lock().unwrap().event_idx = enabled;
        }
    }

    fn update_memory(&mut self, mem: GuestMemoryMmap) -> VhostUserBackendResult<()> {
        debug!("update memory\n");
        for thread in self.threads.iter() {
            thread.lock().unwrap().mem = Some(mem.clone());
        }
        Ok(())
    }

    fn handle_event(
        &self,
        device_event: u16,
        evset: epoll::Events,
        vrings: &[Arc<RwLock<Vring>>],
        thread_id: usize,
    ) -> VhostUserBackendResult<bool> {
        let mut vring_rx = vrings[0].write().unwrap();
        let mut vring_tx = vrings[1].write().unwrap();
        let mut work = true;

        debug!("event received: {:?}", device_event);

        if evset != epoll::Events::EPOLLIN {
            return Err(Error::HandleEventNotEpollIn.into());
        }

        // vm-virtio's Queue implementation only checks avail_index
        // once, so to properly support EVENT_IDX we need to keep
        // calling process_*() until it stops finding new
        // requests on the queue.
        let mut thread = self.threads[thread_id].lock().unwrap();

        while work {
            work = false;

            match device_event {
                RX_QUEUE_EVENT => {
                    debug!("vsock: RX queue event");

                    if thread.unix_backend.has_pending_rx() {
                        work |= thread.process_rx(&mut vring_rx);
                    }
                }
                TX_QUEUE_EVENT => {
                    debug!("vsock: TX queue event");

                    work |= thread.process_tx(&mut vring_tx);
                    if thread.unix_backend.has_pending_rx() {
                        work |= thread.process_rx(&mut vring_rx);
                    }
                }
                EVT_QUEUE_EVENT => {
                    debug!("vsock: EVT queue event");
                }
                BACKEND_EVENT => {
                    debug!("vsock: backend event");
                    thread.unix_backend.notify(evset);

                    work |= thread.process_tx(&mut vring_tx);
                    if thread.unix_backend.has_pending_rx() {
                        work |= thread.process_rx(&mut vring_rx);
                    }
                }
                _ => return Err(Error::HandleEventUnknownEvent.into()),
            }

            if thread.event_idx {
                vring_tx.mut_queue().update_avail_event(thread.mem.as_ref().unwrap());
                vring_rx.mut_queue().update_avail_event(thread.mem.as_ref().unwrap());
            } else {
                work = false;
            }
        }

        Ok(false)
    }

    fn get_config(&self, _offset: u32, _size: u32) -> Vec<u8> {
        debug!("get_config\n");
        // self.config is a statically allocated virtio_vsock_config
        let buf = unsafe {
            slice::from_raw_parts(
                &self.config as *const virtio_vsock_config as *const _,
                mem::size_of::<virtio_vsock_config>(),
            )
        };

        buf.to_vec()
    }

    fn exit_event(&self, thread_index: usize) -> Option<(EventFd, Option<u16>)> {
        // The exit event is placed after the queue and backend event,
        // which is event index 3.
        Some((
            self.threads[thread_index]
                .lock()
                .unwrap()
                .kill_evt
                .try_clone()
                .unwrap(),
            Some(4),
        ))
    }
}

struct VhostUserVsockBackendConfig {
    guest_cid: u32,
    uds_path: String,
    socket: String,
}

impl VhostUserVsockBackendConfig {
    fn parse(backend: &str) -> Result<Self> {
        let mut parser = OptionParser::new();

        parser
            .add("guest_cid")
            .add("uds_path")
            .add("socket");
        parser.parse(backend).map_err(Error::FailedConfigParse)?;

        let guest_cid = parser
            .convert("guest_cid")
            .map_err(Error::FailedConfigParse)?
            .unwrap_or(3);
        let socket = parser.get("socket").ok_or(Error::SocketParameterMissing)?;
        let uds_path = parser.get("uds_path").ok_or(Error::UDSPathParameterMissing)?;

        Ok(VhostUserVsockBackendConfig {
            guest_cid,
            uds_path,
            socket,
        })
    }
}

pub fn start_vsock_backend(backend_command: &str) {
    let backend_config = match VhostUserVsockBackendConfig::parse(backend_command) {
        Ok(config) => config,
        Err(e) => {
            println!("Failed parsing parameters {:?}", e);
            process::exit(1);
        }
    };

    let vsock_backend = Arc::new(RwLock::new(
        VhostUserVsockBackend::new(
            backend_config.guest_cid,
            backend_config.uds_path.to_string(),
        )
        .unwrap(),
    ));

    debug!("vsock_backend is created!\n");

    let listener = Listener::new(&backend_config.socket, true).unwrap();

    let name = "vhost-user-vsock-backend";
    let mut vsock_daemon = VhostUserDaemon::new(
        name.to_string(),
        vsock_backend.clone(),
    )
    .unwrap();

    debug!("vsock_daemon is created!\n");

    let mut vring_workers = vsock_daemon.get_vring_workers();

    if vring_workers.len() != vsock_backend.read().unwrap().threads.len() {
        error!("Number of vring workers must be identical to the number of backend threads");
        process::exit(1);
    }

    for thread in vsock_backend.read().unwrap().threads.iter() {
        thread
            .lock()
            .unwrap()
            .set_vring_worker(Some(vring_workers.remove(0)));
    }

    if let Err(e) = vsock_daemon.start(listener) {
        println!(
            "failed to start daemon for vhost-user-vsock with error: {:?}",
            e
        );
        process::exit(1);
    }

    debug!("vsock_daemon is started!\n");

    if let Err(e) = vsock_daemon.wait() {
        error!("Error from the main thread: {:?}", e);
    }

    debug!("vsock_daemon is finished!\n");

    for thread in vsock_backend.read().unwrap().threads.iter() {
        if let Err(e) = thread.lock().unwrap().kill_evt.write(1) {
            error!("Error shutting down worker thread: {:?}", e)
        }
    }
}
