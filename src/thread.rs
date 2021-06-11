use super::Error;
use super::Result;
use super::BACKEND_EVENT;
use super::QUEUE_SIZE;
use libc::{self, EFD_NONBLOCK};
use log::*;
use std::sync::Arc;
use vhost_user_backend::{Vring, VringWorker};
use virtio_devices::vsock::{VsockChannel, VsockEpollListener, VsockPacket, VsockUnixBackend};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

pub struct VhostUserVsockThread {
    pub mem: Option<GuestMemoryMmap>,
    pub event_idx: bool,
    pub kill_evt: EventFd,
    pub unix_backend: VsockUnixBackend,
    vring_worker: Option<Arc<VringWorker>>,
}

impl VhostUserVsockThread {
    pub fn new(unix_backend: VsockUnixBackend) -> Result<Self> {
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

        self.vring_worker
            .as_ref()
            .unwrap()
            .register_listener(
                self.unix_backend.get_polled_fd(),
                self.unix_backend.get_polled_evset(),
                u64::from(BACKEND_EVENT),
            )
            .unwrap();
    }

    pub fn process_rx(&mut self, vring: &mut Vring) -> bool {
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

    pub fn process_tx(&mut self, vring: &mut Vring) -> bool {
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
