//! A TCP-or-UDP view over the transport header, so NAT rewriting is written once for both.

use smoltcp::wire::{IpAddress, IpProtocol, TcpPacket, UdpPacket};

pub enum L4<'a> {
    Tcp(TcpPacket<&'a mut [u8]>),
    Udp(UdpPacket<&'a mut [u8]>),
}

/// Generates a method that forwards to the same-named method on whichever variant is active.
macro_rules! forward {
    ($(fn $name:ident(&self) -> $ret:ty;)*) => {$(
        pub fn $name(&self) -> $ret {
            match self {
                L4::Tcp(p) => p.$name(),
                L4::Udp(p) => p.$name(),
            }
        }
    )*};
    ($(fn $name:ident(&mut self $(, $arg:ident: $ty:ty)*);)*) => {$(
        pub fn $name(&mut self $(, $arg: $ty)*) {
            match self {
                L4::Tcp(p) => p.$name($($arg),*),
                L4::Udp(p) => p.$name($($arg),*),
            }
        }
    )*};
}

impl<'a> L4<'a> {
    /// Parses `payload` (the IP payload) as the transport header named by `proto`.
    /// None if it's malformed or a protocol we don't translate.
    pub fn parse(proto: IpProtocol, payload: &'a mut [u8]) -> Option<Self> {
        match proto {
            IpProtocol::Tcp => Some(L4::Tcp(TcpPacket::new_checked(payload).ok()?)),
            IpProtocol::Udp => Some(L4::Udp(UdpPacket::new_checked(payload).ok()?)),
            _ => None,
        }
    }

    forward! {
        fn src_port(&self) -> u16;
        fn dst_port(&self) -> u16;
    }

    forward! {
        fn set_src_port(&mut self, port: u16);
        fn set_dst_port(&mut self, port: u16);
        fn fill_checksum(&mut self, src_addr: &IpAddress, dst_addr: &IpAddress);
    }
}
