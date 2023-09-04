use std::net::SocketAddr;

use xelis_common::{serializer::{Serializer, Reader, ReaderError, Writer}, utils::{ip_from_bytes, ip_to_bytes}};

// this packet is sent when a peer disconnects from one of our peer
// it is used to continue to track common peers between us and our peers
// This is used to avoid the problem of not broadcasting a Block propagation
// when we are broadcasting blocks and that we have him in common but that we
// are not connected anymore to it.
#[derive(Debug)]
pub struct PacketPeerDisconnected {
    addr: SocketAddr // outgoing address
}

impl PacketPeerDisconnected {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr
        }
    }

    pub fn get_addr(&self) -> &SocketAddr {
        &self.addr
    }
}

impl Serializer for PacketPeerDisconnected {
    fn read(reader: &mut Reader) -> Result<Self, ReaderError> {
        let addr = ip_from_bytes(reader)?;
        Ok(Self::new(addr))
    }

    fn write(&self, writer: &mut Writer) {
        writer.write_bytes(&ip_to_bytes(&self.addr));
    }
}