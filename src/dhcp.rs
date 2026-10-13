use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use edge_dhcp::server::{Server, ServerOptions};
use edge_nal::UdpBind;
use edge_nal_embassy::{Udp, UdpBuffers};
use embassy_net::Stack;

use crate::config::AP_IP;

#[embassy_executor::task]
pub async fn dhcp_server(stack: Stack<'static>) {
    let ip = AP_IP;

    let buffers = mk_static!(UdpBuffers<1>, UdpBuffers::<1>::new());
    let udp = Udp::new(stack, buffers);
    let mut socket = udp
        .bind(SocketAddr::new(IpAddr::V4([0, 0, 0, 0].into()), 67))
        .await
        .unwrap();
    // TODO 4: let mut buf = [0u8; 1500];
    let mut buf: [u8; 1500] = [0; 1500];
    let mut gw_buf = [Ipv4Addr::UNSPECIFIED];
    // Public resolvers for now; TODO: use the router's DNS from the STA lease
    let dns = [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)];
    let server_options = ServerOptions::new(ip, Some(&mut gw_buf));
    // TEMP bisect: the DNS option was never flashed before; test without it.
    // server_options.dns = &dns;
    let _ = &dns;
    edge_dhcp::io::server::run(
        &mut Server::<_, 64>::new_with_et(ip),
        &server_options,
        &mut socket,
        &mut buf,
    )
    .await
    .unwrap()
}
