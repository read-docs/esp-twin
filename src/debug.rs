use defmt::info;
use smoltcp::wire::{EthernetFrame, IpProtocol, Ipv4Packet, TcpPacket, UdpPacket};

pub fn log_frame(bytes: &[u8]) -> Option<()> {
    let eth = EthernetFrame::new_checked(bytes).ok()?;
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    match ip.next_header() {
        IpProtocol::Tcp => {
            let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
            info!(
                "TCP {:?}:{} -> {:?}:{} syn={} ack={} len={}",
                ip.src_addr(),
                tcp.src_port(),
                ip.dst_addr(),
                tcp.dst_port(),
                tcp.syn(),
                tcp.ack(),
                bytes.len(),
            );
        }
        IpProtocol::Udp => {
            let udp = UdpPacket::new_checked(ip.payload()).ok()?;
            info!(
                "UDP {:?}:{} -> {:?}:{} len={}",
                ip.src_addr(),
                udp.src_port(),
                ip.dst_addr(),
                udp.dst_port(),
                bytes.len(),
            );
        }
        proto => info!(
            "{:?} {:?} -> {:?} len={}",
            proto,
            ip.src_addr(),
            ip.dst_addr(),
            bytes.len(),
        ),
    }
    Some(())
}
