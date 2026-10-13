#![no_std]
#![no_main]

use core::net::Ipv4Addr;
use core::sync::atomic::Ordering;

use defmt::{error, info};
use embassy_executor::Spawner;
use embassy_net::driver::{Driver, HardwareAddress};
use embassy_net::{Ipv4Cidr, StackResources, StaticConfigV4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::zerocopy_channel::Channel;
use embassy_time::{Duration, Timer};
use esp_hal::{clock::CpuClock, timer::timg::TimerGroup};
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ControllerConfig, WifiController, ap::AccessPointConfig,
    sta::StationConfig,
};
use esp_twin::config::{AP_IP, AP_SSID, GATEWAY_MAC, PASSWORD, SSID, parse_mac};
use esp_twin::driver::{NatDriver, StaDriver};
use esp_twin::frame::Frame;
use esp_twin::nat::NAT_DROPS;
use esp_twin::{dhcp, mk_static, tasks, wifi};
use smoltcp::wire::EthernetAddress;
use static_cell::ConstStaticCell;

extern crate alloc;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    error!("{}", info);
    loop {}
}

esp_bootloader_esp_idf::esp_app_desc!();

/// Number of frame slots in each inter-stack channel.
const FRAME_SLOTS: usize = 8;

type FrameChannel = Channel<'static, CriticalSectionRawMutex, Frame>;

/// Leaks a fresh frame buffer into a channel. Must be called once per call site (see `mk_static!`).
macro_rules! frame_channel {
    () => {{
        static BUF: ConstStaticCell<[Frame; FRAME_SLOTS]> =
            ConstStaticCell::new([Frame::EMPTY; FRAME_SLOTS]);
        mk_static!(FrameChannel, Channel::new(BUF.take()))
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    info!("Runtime up");

    // AP rx -> NAT task, NAT task -> STA tx, STA rx -> AP tx.
    let (send_half, receive_half) = frame_channel!().split();
    let (to_sta, from_nat) = frame_channel!().split();
    let (to_ap, from_sta) = frame_channel!().split();

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

    spawner.spawn(wifi::connection(wifi_controller).unwrap());
    spawner.spawn(tasks::net_task(sta_runner).unwrap());
    spawner.spawn(tasks::nat_net_task(ap_runner).unwrap());
    spawner
        .spawn(tasks::nat_log_task(receive_half, to_sta, sta_stack, sta_mac, gateway_mac).unwrap());
    spawner.spawn(tasks::sta_lease_log(sta_stack).unwrap());
    spawner.spawn(dhcp::dhcp_server(ap_stack).unwrap());

    loop {
        Timer::after(Duration::from_secs(5)).await;
        info!("Nat drops: {}", NAT_DROPS.load(Ordering::Relaxed));
    }
}
