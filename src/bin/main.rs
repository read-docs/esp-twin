#![no_std]
#![no_main]

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use defmt::{error, info};
use edge_dhcp::server::{Server, ServerOptions};
use edge_nal::UdpBind;
use edge_nal_embassy::{Udp, UdpBuffers};
use embassy_executor::Spawner;
use embassy_futures::select::Either;
use embassy_net::{Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4};
use embassy_time::{Duration, Timer};
use esp_hal::{clock::CpuClock, timer::timg::TimerGroup};
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ControllerConfig, Interface, WifiController,
    ap::AccessPointConfig, sta::StationConfig,
};

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
esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const AP_SSID: &str = env!("AP_SSID");

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    info!("Runtime up");

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
    let wifi_sta_device = esp_radio::wifi::Interface::station();
    let wifi_controller = WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(wifi_config),
    )
    .expect("wifi init");

    let ap_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Addr::new(192, 168, 2, 1), 24),
        gateway: Some(Ipv4Addr::new(192, 168, 2, 1)),
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
    spawner.spawn(net_task(ap_runner).unwrap());
    spawner.spawn(dhcp_server(ap_stack).unwrap());

    loop {
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task(pool_size = 2)]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

#[embassy_executor::task]
async fn dhcp_server(stack: Stack<'static>) {
    let ip = Ipv4Addr::new(192, 168, 2, 1);

    // TODO 1: static buffer pool for the UDP sockets (edge_nal_embassy::UdpBuffers<N, TX, RX, META>)
    let buffers = mk_static!(UdpBuffers<1>, UdpBuffers::<1>::new());
    let udp = Udp::new(stack, buffers);
    // TODO 3: bind a socket to 0.0.0.0:67 (DEFAULT_SERVER_PORT) via edge_nal::UdpBind
    let mut socket = udp
        .bind(SocketAddr::new(
            IpAddr::V4([0, 0, 0, 0].try_into().unwrap()),
            67,
        ))
        .await
        .unwrap();
    // TODO 4: let mut buf = [0u8; 1500];
    let mut buf: [u8; 1500] = [0; 1500];
    // TODO 5: let mut gw_buf = [Ipv4Addr::UNSPECIFIED];
    let mut gw_buf = [Ipv4Addr::UNSPECIFIED];
    // Public resolvers for now; TODO: use the router's DNS from the STA lease
    let dns = [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)];
    let mut server_options = ServerOptions::new(ip, Some(&mut gw_buf));
    server_options.dns = &dns;
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
