use smoltcp::wire::{
    EthernetAddress, EthernetFrame, IpAddress, IpProtocol, Ipv4Address, Ipv4Packet, UdpPacket,
};

use embassy_time::Instant;

use super::{NAT_TABLE, NatEntry};

/// Rewrites a client's outbound UDP frame in place so it looks like it came from the ESP's STA.
/// Returns the translated source port on success, None if the frame isn't something we handle yet.
pub fn rewrite_outbound(
    frame: &mut [u8],
    sta_mac: EthernetAddress,
    sta_ip: Ipv4Address,
    gateway_mac: EthernetAddress,
) -> Option<u16> {
    // 1. Parse the Ethernet frame (mutable). Bail out with None if it's malformed.
    let mut eth = EthernetFrame::new_checked(&mut frame[..]).ok()?;

    // 2. Remember the client's MAC BEFORE overwriting it: the NAT table needs it.
    let client_mac = eth.src_addr();

    // 3. Parse the IPv4 packet inside; only handle UDP for now.
    let mut ip = Ipv4Packet::new_checked(eth.payload_mut()).ok()?;
    if ip.next_header() != IpProtocol::Udp {
        return None;
    }

    // 4. Remember client_ip and dst_ip: the UDP checksum covers src+dst (pseudo-header).
    let client_ip = ip.src_addr();
    let dst_ip = ip.dst_addr();

    // 5. Parse the UDP datagram inside the IP payload.
    let mut udp = UdpPacket::new_checked(ip.payload_mut()).ok()?;
    let client_port = udp.src_port();

    // 6. Pick the translated port and insert/refresh the NAT table entry
    //    (key: translated port, value: client_mac, client_ip, client_port).
    //    TODO: what if the table is full, or the key already belongs to another client?
    let nat_port: u16 = 40000 + (client_port % 20000);

    NAT_TABLE.lock(|t| {
        t.borrow_mut()
            .insert(
                nat_port,
                NatEntry {
                    client_mac,
                    client_ip,
                    client_port,
                    last_seen: Instant::now(),
                },
            )
            .ok()
    });

    // 7. Rewrite the UDP source port, then recompute the UDP checksum.
    //    The pseudo-header uses the NEW src IP, so pass sta_ip (not client_ip).
    udp.set_src_port(nat_port);
    udp.fill_checksum(&IpAddress::Ipv4(sta_ip), &IpAddress::Ipv4(dst_ip));

    // 8. Rewrite the IPv4 source address and recompute the IPv4 header checksum.
    ip.set_src_addr(sta_ip);
    ip.fill_checksum();

    // 9. Rewrite the Ethernet src/dst.
    eth.set_src_addr(sta_mac);
    eth.set_dst_addr(gateway_mac);

    Some(nat_port)
}
