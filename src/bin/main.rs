#![no_std]
#![no_main]

use core::cell::RefCell;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::Context;

use defmt::{error, info};
use edge_dhcp::server::{Server, ServerOptions};
use edge_nal::UdpBind;
use edge_nal_embassy::{Udp, UdpBuffers};
use embassy_executor::Spawner;
use embassy_futures::select::Either;
use embassy_net::driver::{Capabilities, HardwareAddress, LinkState, RxToken, TxToken};
use embassy_net::{Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4, driver::Driver};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::waitqueue::AtomicWaker;
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
use heapless::index_map::FnvIndexMap;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, IpAddress, IpProtocol, Ipv4Address,
    Ipv4Packet, TcpPacket, UdpPacket,
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

struct StaDriver<D: Driver> {
    inner: D,
    from_nat: Receiver<'static, CriticalSectionRawMutex, Frame>,
    to_ap: Sender<'static, CriticalSectionRawMutex, Frame>,
    ap_mac: EthernetAddress,
}

impl<D: Driver> StaDriver<D> {
    fn drain_nat(&mut self, cx: &mut Context) {
        while let Some(frame) = self.from_nat.try_receive() {
            let Some(tx) = self.inner.transmit(cx) else {
                break;
            };
            info!("STA tx: {} bytes", frame.len);
            tx.consume(frame.len, |buf| {
                buf.copy_from_slice(&frame.data[..frame.len])
            });
            self.from_nat.receive_done();
        }
    }
}

impl<D: Driver> Driver for StaDriver<D> {
    type TxToken<'a>
        = D::TxToken<'a>
    where
        Self: 'a;
    type RxToken<'a>
        = StaRxToken<'a, D::RxToken<'a>>
    where
        Self: 'a;

    fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        STA_WAKER.register(cx.waker());
        self.drain_nat(cx);
        let to_ap = &mut self.to_ap;
        let ap_mac = self.ap_mac;
        self.inner.receive(cx).map(|(rx, tx)| {
            (
                StaRxToken {
                    inner: rx,
                    to_ap,
                    ap_mac,
                },
                tx,
            )
        })
    }
    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        STA_WAKER.register(cx.waker());
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

struct StaRxToken<'a, R: RxToken> {
    inner: R,
    to_ap: &'a mut Sender<'static, CriticalSectionRawMutex, Frame>,
    ap_mac: EthernetAddress,
}

impl<R: RxToken> RxToken for StaRxToken<'_, R> {
    fn consume<T, F: FnOnce(&mut [u8]) -> T>(self, f: F) -> T {
        let to_ap = self.to_ap;
        let ap_mac = self.ap_mac;
        self.inner.consume(|frame| {
            if classify_inbound(frame).is_some() {
                forward_inbound(to_ap, frame, ap_mac);
                f(&mut [])
            } else {
                f(frame)
            }
        })
    }
}

struct NatDriver<D: Driver> {
    inner: D,
    to_nat: Sender<'static, CriticalSectionRawMutex, Frame>,
    from_sta: Receiver<'static, CriticalSectionRawMutex, Frame>,
}

impl<D: Driver> NatDriver<D> {
    /// Send every waiting rewritten reply out the AP radio, as long as it has room.
    fn drain_sta(&mut self, cx: &mut Context) {
        while let Some(frame) = self.from_sta.try_receive() {
            let Some(tx) = self.inner.transmit(cx) else {
                break;
            };
            info!("AP tx: {} bytes", frame.len);
            tx.consume(frame.len, |buf| {
                buf.copy_from_slice(&frame.data[..frame.len])
            });
            self.from_sta.receive_done();
        }
    }
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
        AP_WAKER.register(cx.waker());
        self.drain_sta(cx);
        let to_nat = &mut self.to_nat;
        self.inner
            .receive(cx)
            .map(|(rx, tx)| (NatRxToken { inner: rx, to_nat }, tx))
    }
    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        AP_WAKER.register(cx.waker());
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

#[derive(Clone, Copy)]
struct NatEntry {
    client_mac: EthernetAddress,
    client_ip: Ipv4Address,
    client_port: u16,
}

enum Verdict {
    Stack,
    Nat,
}

esp_bootloader_esp_idf::esp_app_desc!();
static NAT_TABLE: Mutex<CriticalSectionRawMutex, RefCell<FnvIndexMap<u16, NatEntry, 16>>> =
    Mutex::new(RefCell::new(FnvIndexMap::new()));
static STA_WAKER: AtomicWaker = AtomicWaker::new();
static AP_WAKER: AtomicWaker = AtomicWaker::new();
static NAT_DROPS: AtomicU32 = AtomicU32::new(0);
const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const AP_SSID: &str = env!("AP_SSID");
const GATEWAY_MAC: &str = env!("GATEWAY_MAC");
const AP_IP: Ipv4Address = Ipv4Address::new(192, 168, 2, 1);
const AP_NET: Ipv4Cidr = Ipv4Cidr::new(AP_IP, 24);

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

    static NAT_FRAME_BUF: ConstStaticCell<[Frame; 8]> = ConstStaticCell::new(
        [const {
            Frame {
                data: [0; 1514],
                len: 0,
            }
        }; 8],
    );

    let nat_buf: &'static mut [Frame; 8] = NAT_FRAME_BUF.take();

    let nat_frame_channel = mk_static!(
        Channel<'static, CriticalSectionRawMutex, Frame>,
        Channel::new(nat_buf)
    );
    let (to_sta, from_nat) = nat_frame_channel.split();

    static AP_FRAME_BUF: ConstStaticCell<[Frame; 8]> = ConstStaticCell::new(
        [const {
            Frame {
                data: [0; 1514],
                len: 0,
            }
        }; 8],
    );
    let ap_buf: &'static mut [Frame; 8] = AP_FRAME_BUF.take();
    let ap_frame_channel = mk_static!(
        Channel<'static, CriticalSectionRawMutex, Frame>,
        Channel::new(ap_buf)
    );
    let (to_ap, from_sta) = ap_frame_channel.split();

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
    let HardwareAddress::Ethernet(ap_mac) = wifi_ap_device.hardware_address() else {
        panic!("AP device has no Ethernet address");
    };
    let ap_mac = EthernetAddress(ap_mac);
    let wifi_ap_device = NatDriver {
        inner: wifi_ap_device,
        to_nat: send_half,
        from_sta,
    };
    let wifi_sta_device = StaDriver {
        inner: esp_radio::wifi::Interface::station(),
        from_nat,
        to_ap,
        ap_mac,
    };
    // Read before the driver moves into embassy_net::new.
    let HardwareAddress::Ethernet(sta_mac) = wifi_sta_device.hardware_address() else {
        panic!("STA device has no Ethernet address");
    };
    let sta_mac = EthernetAddress(sta_mac);
    let gateway_mac = parse_mac(GATEWAY_MAC);
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
    spawner.spawn(nat_log_task(receive_half, to_sta, sta_stack, sta_mac, gateway_mac).unwrap());
    spawner.spawn(sta_lease_log(sta_stack).unwrap());
    spawner.spawn(dhcp_server(ap_stack).unwrap());

    loop {
        Timer::after(Duration::from_secs(5)).await;
        info!("Nat drops: {}", NAT_DROPS.load(Ordering::Relaxed));
    }
}

//#[embassy_executor::task(pool_size = 2)]
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, StaDriver<Interface>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn nat_net_task(mut runner: Runner<'static, NatDriver<Interface>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn nat_log_task(
    mut rx: Receiver<'static, CriticalSectionRawMutex, Frame>,
    mut to_sta: Sender<'static, CriticalSectionRawMutex, Frame>,
    sta_stack: Stack<'static>,
    sta_mac: EthernetAddress,
    gateway_mac: EthernetAddress,
) {
    loop {
        let frame = rx.receive().await;
        log_frame(&frame.data[..frame.len]);
        let len = frame.len;

        // No router lease yet: nothing to translate to, so drop.
        let Some(sta_ip) = sta_stack.config_v4().map(|c| c.address.address()) else {
            NAT_DROPS.fetch_add(1, Ordering::Relaxed);
            rx.receive_done();
            continue;
        };
        let Some(slot) = to_sta.try_send() else {
            NAT_DROPS.fetch_add(1, Ordering::Relaxed);
            rx.receive_done();
            continue;
        };

        // Rewrite in the outgoing slot itself. Only frames the rewrite accepted (UDP) are sent;
        // everything else is dropped. An unsent slot is simply reused by the next try_send.
        slot.data[..len].copy_from_slice(&frame.data[..len]);
        match rewrite_outbound(&mut slot.data[..len], sta_mac, sta_ip, gateway_mac) {
            Some(nat_port) => {
                info!("NAT out: translated port {} len={}", nat_port, len);
                slot.len = len;
                to_sta.send_done();
                STA_WAKER.wake(); // poke the runner
            }
            None => {
                NAT_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }
        rx.receive_done();
    }
}

fn parse_mac(s: &str) -> EthernetAddress {
    let mut bytes = [0u8; 6];
    let mut parts = s.split(':');
    for b in bytes.iter_mut() {
        *b = u8::from_str_radix(parts.next().expect("MAC too short"), 16).expect("bad MAC hex");
    }
    assert!(parts.next().is_none(), "MAC too long");
    EthernetAddress(bytes)
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
async fn sta_lease_log(stack: Stack<'static>) {
    loop {
        stack.wait_config_up().await;
        info!("{:?}", stack.config_v4());
        stack.wait_config_down().await;
    }
}

#[embassy_executor::task]
async fn dhcp_server(stack: Stack<'static>) {
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

/// Rewrites a client's outbound UDP frame in place so it looks like it came from the ESP's STA.
/// Returns the translated source port on success, None if the frame isn't something we handle yet.
fn rewrite_outbound(
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
                },
            )
            .ok()
    });

    // 7. Rewrite the UDP source port, then recompute the UDP checksum.
    //    The pseudo-header uses the NEW src IP, so pass sta_ip (not client_ip).
    udp.set_src_port(nat_port);
    udp.fill_checksum(&IpAddress::Ipv4(sta_ip), &IpAddress::Ipv4(dst_ip));

    // 8. Rewrite the IPv4 source address and recompute the IPv4 header checksum.
    //    TODO: `udp` borrows from `ip`. What must happen to `udp` before you touch `ip` again?
    //    (Hint: scope the inner borrows in a `{ ... }` block.)
    ip.set_src_addr(sta_ip);
    ip.fill_checksum();

    // 9. Rewrite the Ethernet src/dst. Same borrow question for `ip` and `eth`.
    eth.set_src_addr(sta_mac);
    eth.set_dst_addr(gateway_mac);

    let _ = (client_mac, client_ip, client_port);
    Some(nat_port)
}

/// If `packet` is a UDP datagram addressed to a translated port in the NAT table, returns that port.
fn classify_inbound(packet: &[u8]) -> Option<u16> {
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
    let Some(entry) = NAT_TABLE.lock(|t| t.borrow().get(&nat_port).copied()) else {
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
fn forward_inbound(
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
