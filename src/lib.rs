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
extern crate vhost;
extern crate vhost_user_backend;
extern crate vm_virtio;

use log::*;
use option_parser::{OptionParser, OptionParserError};
use std::fmt;
use std::io::{self};
use std::process;
use std::sync::{Arc, RwLock};
use vhost::vhost_user::Error as VhostUserError;
use vhost::vhost_user::Listener;
use vhost_user_backend::VhostUserDaemon;
use virtio_bindings::bindings::virtio_ring::__u64;
use virtio_devices::vsock::VsockUnixError;
use virtio_devices::DeviceEventT;

mod backend;
use backend::VhostUserVsockBackend;

mod thread;
use thread::VhostUserVsockThread;

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
    CreateVsockBackend(VsockUnixError),
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

struct VhostUserVsockBackendConfig {
    guest_cid: u32,
    uds_path: String,
    socket: String,
}

impl VhostUserVsockBackendConfig {
    fn parse(backend: &str) -> Result<Self> {
        let mut parser = OptionParser::new();

        parser.add("guest_cid").add("uds_path").add("socket");
        parser.parse(backend).map_err(Error::FailedConfigParse)?;

        let guest_cid = parser
            .convert("guest_cid")
            .map_err(Error::FailedConfigParse)?
            .unwrap_or(3);
        let socket = parser.get("socket").ok_or(Error::SocketParameterMissing)?;
        let uds_path = parser
            .get("uds_path")
            .ok_or(Error::UDSPathParameterMissing)?;

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
    let mut vsock_daemon = VhostUserDaemon::new(name.to_string(), vsock_backend.clone()).unwrap();

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
