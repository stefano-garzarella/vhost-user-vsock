// Copyright 2020 Red Hat, Inc. All Rights Reserved.
//
// Portions Copyright 2019 Intel Corporation. All Rights Reserved.
//
// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)

#[macro_use(crate_version, crate_authors)]
extern crate clap;
extern crate vhost_user_vsock;

use clap::{App, Arg};
use vhost_user_vsock::start_vsock_backend;

fn main() {
    let cmd_arguments = App::new("vhost-user-vsock backend")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Launch a vhost-user-vsock backend.")
        .arg(
            Arg::with_name("vsock-backend")
                .long("vsock-backend")
                .help(
                    "vhost-user-vsock backend parameters \
                     \"guest-cid=<guest_cid>,uds_path=<uds_path>,sock=<socket_path>\"",
                )
                .takes_value(true)
                .min_values(1),
        )
        .get_matches();

    let backend_command = cmd_arguments.value_of("vsock-backend").unwrap();
    start_vsock_backend(backend_command);
}
