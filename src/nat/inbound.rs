use core::sync::atomic::Ordering;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::zerocopy_channel::Sender;
use embassy_time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, IpAddress, IpProtocol, Ipv4Packet, UdpPacket,
};

use super::{NAT_DROPS, NAT_TABLE};
use crate::driver::AP_WAKER;
use crate::frame::Frame;

/// If `packet` is a UDP datagram addressed to a translated port in the NAT table, returns that port.
pub fn classify_inbound(packet: &[u8]) -> Option<u16> {
    let eth = EthernetFrame::new_checked(packet).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Udp {
        return None;
    }
    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
    let port = udp.dst_port();
    NAT_TABLE
        .lock(|t| t.borrow().contains_key(&port))
        .then_some(port)
}

/// Rewrites a router reply (to a translated port) so it addresses the original client.
/// Returns false if the NAT entry vanished or the frame can't be parsed.
fn rewrite_inbound(frame: &mut [u8], ap_mac: EthernetAddress) -> bool {
    let Ok(mut eth) = EthernetFrame::new_checked(&mut frame[..]) else {
        return false;
    };
    let Ok(mut ip) = Ipv4Packet::new_checked(eth.payload_mut()) else {
        return false;
    };
    let src_ip = ip.src_addr();
    let Ok(mut udp) = UdpPacket::new_checked(ip.payload_mut()) else {
        return false;
    };
    let nat_port = udp.dst_port();
    let is_dns = udp.src_port() == 53;
    let entry = NAT_TABLE.lock(|t| {
        let mut t = t.borrow_mut();
        if is_dns {
            t.remove(&nat_port) // one query, one reply: free the slot
        } else {
            // QUIC/TCP etc: many replies, keep the entry and mark it alive
            t.get_mut(&nat_port).map(|e| {
                e.last_seen = Instant::now();
                *e
            })
        }
    });
    let Some(entry) = entry else {
        return false;
    };

    udp.set_dst_port(entry.client_port);
    udp.fill_checksum(&IpAddress::Ipv4(src_ip), &IpAddress::Ipv4(entry.client_ip));
    ip.set_dst_addr(entry.client_ip);
    ip.fill_checksum();
    eth.set_src_addr(ap_mac);
    eth.set_dst_addr(entry.client_mac);
    true
}

/// Copies a reply into the AP-bound channel, rewrites it there, and wakes the AP runner.
pub fn forward_inbound(
    to_ap: &mut Sender<'static, CriticalSectionRawMutex, Frame>,
    packet: &[u8],
    ap_mac: EthernetAddress,
) {
    let len = packet.len();
    let Some(slot) = to_ap.try_send() else {
        NAT_DROPS.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if len > slot.data.len() {
        NAT_DROPS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    slot.data[..len].copy_from_slice(packet);
    if rewrite_inbound(&mut slot.data[..len], ap_mac) {
        slot.len = len;
        to_ap.send_done();
        AP_WAKER.wake();
    } else {
        NAT_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}
