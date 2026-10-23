// SPDX-License-Identifier: AGPL-3.0-only

//! Helpers shared by the integration tests.

use std::net::{Ipv4Addr, SocketAddr};

use socket2::{Domain, Socket, Type};

/// A loopback port that refuses connections for as long as the value lives.
///
/// The socket is bound and never put into the listening state, so a SYN to
/// it is answered with a reset, and no other socket (a sibling test's server
/// on port 0, say) can be handed the port. Binding and dropping a listener
/// instead gives the port back to the next bind on port 0.
pub struct RefusedPort {
    _socket: Socket,
    address: SocketAddr,
}

impl RefusedPort {
    pub fn new() -> Self {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).expect("socket");
        let wanted = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        socket.bind(&wanted.into()).expect("binds");
        let address = socket
            .local_addr()
            .expect("has an address")
            .as_socket()
            .expect("an IP address");
        Self {
            _socket: socket,
            address,
        }
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}
