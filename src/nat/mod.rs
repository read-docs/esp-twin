//! NAT: classifying frames and rewriting them between the AP (client) and STA (router) sides.

mod inbound;
#[expect(
    dead_code,
    reason = "TCP/UDP abstraction, not wired into the rewrite path yet"
)]
mod l4;
mod outbound;

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::zerocopy_channel::Sender;
use embassy_time::Instant;
use heapless::index_map::FnvIndexMap;
use smoltcp::wire::{EthernetAddress, EthernetFrame, EthernetProtocol, Ipv4Address, Ipv4Packet};

use crate::config::AP_NET;
use crate::frame::Frame;

pub use inbound::{classify_inbound, forward_inbound};
pub use outbound::rewrite_outbound;

#[derive(Clone, Copy)]
pub struct NatEntry {
    pub client_mac: EthernetAddress,
    pub client_ip: Ipv4Address,
    pub client_port: u16,
    pub last_seen: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Proto {
    Tcp,
    Udp,
}

pub struct NatKey {
    pub proto: Proto,
    pub nat_port: u16,
}

pub enum Verdict {
    Stack,
    Nat,
}

pub(crate) static NAT_TABLE: Mutex<
    CriticalSectionRawMutex,
    RefCell<FnvIndexMap<u16, NatEntry, 64>>,
> = Mutex::new(RefCell::new(FnvIndexMap::new()));
pub static NAT_DROPS: AtomicU32 = AtomicU32::new(0);

pub fn classify_packet(packet: &[u8]) -> Verdict {
    // Frames we can't parse are left to the stack (it will drop them); never panic in the RX path.
    let Ok(eth) = EthernetFrame::new_checked(packet) else {
        return Verdict::Stack;
    };
    // Non-IPv4 (ARP, IPv6, ...) must reach the stack, e.g. ARP for 192.168.2.1.
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return Verdict::Stack;
    }
    let Ok(ip) = Ipv4Packet::new_checked(eth.payload()) else {
        return Verdict::Stack;
    };
    let dst = ip.dst_addr();
    if AP_NET.contains_addr(&dst)
        || dst.is_broadcast()
        || dst.is_multicast()
        || dst.is_unspecified()
    {
        Verdict::Stack
    } else {
        Verdict::Nat
    }
}

pub fn divert(tx: &mut Sender<'static, CriticalSectionRawMutex, Frame>, packet: &[u8]) {
    if packet.len() > 1514 {
        NAT_DROPS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some(slot) = tx.try_send() else {
        NAT_DROPS.fetch_add(1, Ordering::Relaxed);
        return;
    };
    slot.data[..packet.len()].copy_from_slice(packet);
    slot.len = packet.len();
    tx.send_done();
}
