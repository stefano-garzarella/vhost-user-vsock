use super::Error;
use super::VhostUserVsockThread;
use super::{Result, VhostUserBackendResult};
use super::{BACKEND_EVENT, EVT_QUEUE_EVENT, RX_QUEUE_EVENT, TX_QUEUE_EVENT};
use super::{NUM_QUEUES, QUEUE_SIZE};
use log::*;
use std::mem;
use std::slice;
use std::sync::{Arc, Mutex, RwLock};
use std::vec::Vec;
use vhost::vhost_user::message::*;
use vhost_user_backend::{VhostUserBackend, Vring};
use virtio_bindings::bindings::virtio_net::*;
use virtio_bindings::bindings::virtio_ring::__u64;
use virtio_bindings::bindings::virtio_ring::*;
use virtio_devices::vsock::{VsockChannel, VsockEpollListener, VsockUnixBackend};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

//TODO: move in the virtio bindings
#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct virtio_vsock_config {
    pub guest_cid: __u64,
}

pub struct VhostUserVsockBackend {
    pub threads: Vec<Mutex<VhostUserVsockThread>>,
    config: virtio_vsock_config,
    queues_per_thread: Vec<u64>,
}

impl VhostUserVsockBackend {
    /// Create a new virtio vsock device with the given guest_cid
    pub fn new(guest_cid: u32, uds_path: String) -> Result<Self> {
        let config = virtio_vsock_config {
            guest_cid: guest_cid.into(),
        };

        let mut queues_per_thread = Vec::new();
        let mut threads = Vec::new();

        let unix_backend =
            VsockUnixBackend::new(guest_cid.into(), uds_path).map_err(Error::CreateVsockBackend)?;

        let thread = Mutex::new(VhostUserVsockThread::new(unix_backend)?);
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
                vring_tx
                    .mut_queue()
                    .update_avail_event(thread.mem.as_ref().unwrap());
                vring_rx
                    .mut_queue()
                    .update_avail_event(thread.mem.as_ref().unwrap());
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
