#![no_std]
#![no_main]

use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::Context;

use defmt::{error, info};
use edge_dhcp::server::{Server, ServerOptions};
use edge_nal::UdpBind;
use edge_nal_embassy::{Udp, UdpBuffers};
use embassy_executor::Spawner;
use embassy_futures::select::Either;
use embassy_net::driver::{self, Capabilities, HardwareAddress, LinkState, RxToken};
use embassy_net::{Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4, driver::Driver};
use embassy_sync::zerocopy_channel::Receiver;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    zerocopy_channel::{Channel, Sender},
};
use embassy_time::{Duration, Timer};
use esp_hal::{clock::CpuClock, timer::timg::TimerGroup};
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ControllerConfig, Interface, WifiController,
    ap::AccessPointConfig, sta::StationConfig,
};
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Address, Ipv4Packet,
    TcpPacket, UdpPacket,
};
use static_cell::ConstStaticCell;

extern crate alloc;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    error!("{}", info);
    loop {}
}

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

struct NatDriver<D: Driver> {
    inner: D,
    to_nat: Sender<'static, CriticalSectionRawMutex, Frame>,
}

impl<D: Driver> Driver for NatDriver<D> {
    type RxToken<'a>
        = NatRxToken<'a, D::RxToken<'a>>
    where
        Self: 'a;
    type TxToken<'a>
        = D::TxToken<'a>
    where
        Self: 'a;
    fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let to_nat = &mut self.to_nat;
        self.inner
            .receive(cx)
            .map(|(rx, tx)| (NatRxToken { inner: rx, to_nat }, tx))
    }
    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        self.inner.transmit(cx)
    }
    fn link_state(&mut self, cx: &mut Context) -> LinkState {
        self.inner.link_state(cx)
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn hardware_address(&self) -> HardwareAddress {
        self.inner.hardware_address()
    }
}

struct NatRxToken<'a, R: embassy_net::driver::RxToken> {
    inner: R,
    to_nat: &'a mut Sender<'static, CriticalSectionRawMutex, Frame>,
}

impl<R: RxToken> RxToken for NatRxToken<'_, R> {
    fn consume<T, F: FnOnce(&mut [u8]) -> T>(self, f: F) -> T {
        let to_nat = self.to_nat;
        self.inner.consume(|frame| match classify_packet(frame) {
            Verdict::Stack => f(frame),
            Verdict::Nat => {
                divert(to_nat, frame);
                f(&mut [])
            }
        })
    }
}

struct Frame {
    data: [u8; 1514],
    len: usize,
}

enum Verdict {
    Stack,
    Nat,
}

esp_bootloader_esp_idf::esp_app_desc!();

static NAT_DROPS: AtomicU32 = AtomicU32::new(0);
const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const AP_SSID: &str = env!("AP_SSID");
const AP_IP: Ipv4Address = Ipv4Address::new(192, 168, 2, 1);

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    info!("Runtime up");

    static FRAME_BUF: ConstStaticCell<[Frame; 8]> = ConstStaticCell::new(
        [const {
            Frame {
                data: [0; 1514],
                len: 0,
            }
        }; 8],
    );

    let buf: &'static mut [Frame; 8] = FRAME_BUF.take();

    let frame_channel = mk_static!(
        Channel<'static, CriticalSectionRawMutex, Frame>,
        Channel::new(buf)
    );
    let (send_half, receive_half) = frame_channel.split();

    // Build the config AND hand it to the controller
    let wifi_config = Config::AccessPointStation(
        StationConfig::default()
            .with_ssid(SSID.try_into().expect("invalid SSID"))
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                PASSWORD.try_into().expect("invalid password"),
            )),
        AccessPointConfig::default()
            .with_ssid(AP_SSID.try_into().expect("invalid AP_SSID"))
            .with_authentication(AuthenticationMethodConfig::Open),
    );
    let wifi_ap_device = esp_radio::wifi::Interface::access_point();
    let wifi_ap_device = NatDriver {
        inner: wifi_ap_device,
        to_nat: send_half,
    };
    let wifi_sta_device = esp_radio::wifi::Interface::station();
    let wifi_controller = WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(wifi_config),
    )
    .expect("wifi init");

    let ap_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Addr::new(192, 168, 2, 1), 24),
        gateway: Some(AP_IP),
        dns_servers: Default::default(),
    });

    let sta_config = embassy_net::Config::dhcpv4(Default::default());

    let rng = esp_hal::rng::Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (ap_stack, ap_runner) = embassy_net::new(
        wifi_ap_device,
        ap_config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );
    let (sta_stack, sta_runner) = embassy_net::new(
        wifi_sta_device,
        sta_config,
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        seed,
    );

    spawner.spawn(connection(wifi_controller).unwrap());
    spawner.spawn(net_task(sta_runner).unwrap());
    spawner.spawn(nat_net_task(ap_runner).unwrap());
    spawner.spawn(nat_log_task(receive_half).unwrap());
    spawner.spawn(dhcp_server(ap_stack).unwrap());

    loop {
        Timer::after(Duration::from_secs(5)).await;
        info!("Nat drops: {}", NAT_DROPS.load(Ordering::Relaxed));
    }
}

//#[embassy_executor::task(pool_size = 2)]
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

#[embassy_executor::task]
async fn nat_net_task(mut runner: Runner<'static, NatDriver<Interface>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn nat_log_task(mut rx: Receiver<'static, CriticalSectionRawMutex, Frame>) {
    loop {
        let frame = rx.receive().await;
        log_frame(&frame.data[..frame.len]);
        // Release the slot, otherwise the next receive() hands back this same frame.
        rx.receive_done();
    }
}

fn log_frame(bytes: &[u8]) -> Option<()> {
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

#[embassy_executor::task]
async fn dhcp_server(stack: Stack<'static>) {
    let ip = AP_IP;

    let buffers = mk_static!(UdpBuffers<1>, UdpBuffers::<1>::new());
    let udp = Udp::new(stack, buffers);
    let mut socket = udp
        .bind(SocketAddr::new(
            IpAddr::V4([0, 0, 0, 0].try_into().unwrap()),
            67,
        ))
        .await
        .unwrap();
    // TODO 4: let mut buf = [0u8; 1500];
    let mut buf: [u8; 1500] = [0; 1500];
    let mut gw_buf = [Ipv4Addr::UNSPECIFIED];
    // Public resolvers for now; TODO: use the router's DNS from the STA lease
    let dns = [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)];
    let mut server_options = ServerOptions::new(ip, Some(&mut gw_buf));
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

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");

    loop {
        match controller.connect_async().await {
            Ok(_) => {
                // wait until we're no longer connected
                loop {
                    let info = embassy_futures::select::select(
                        controller.wait_for_disconnect_async(),
                        controller.wait_for_access_point_connected_event_async(),
                    )
                    .await;

                    match info {
                        Either::First(station_disconnected) => {
                            if let Ok(station_disconnected) = station_disconnected {
                                info!("Station disconnected: {:?}", station_disconnected);
                            }
                            break;
                        }
                        Either::Second(event) => {
                            if let Ok(event) = event {
                                match event {
                                    esp_radio::wifi::ap::EventInfo::Connected(info) => {
                                        info!("Station connected: {:?}", info);
                                    }
                                    esp_radio::wifi::ap::EventInfo::Disconnected(info) => {
                                        info!("Station disconnected: {:?}", info);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to connect to wifi: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

fn classify_packet(packet: &[u8]) -> Verdict {
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
    if dst == AP_IP || dst.is_broadcast() || dst.is_multicast() || dst.is_unspecified() {
        Verdict::Stack
    } else {
        Verdict::Nat
    }
}

fn divert(tx: &mut Sender<'static, CriticalSectionRawMutex, Frame>, packet: &[u8]) {
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
