use core::sync::atomic::Ordering;

use defmt::info;
use embassy_net::{Runner, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::zerocopy_channel::{Receiver, Sender};
use esp_radio::wifi::Interface;
use smoltcp::wire::EthernetAddress;

use crate::debug::log_frame;
use crate::driver::{NatDriver, STA_WAKER, StaDriver};
use crate::frame::Frame;
use crate::nat::{NAT_DROPS, rewrite_outbound};

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, StaDriver<Interface>>) {
    runner.run().await
}

#[embassy_executor::task]
pub async fn nat_net_task(mut runner: Runner<'static, NatDriver<Interface>>) {
    runner.run().await
}

#[embassy_executor::task]
pub async fn nat_log_task(
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

#[embassy_executor::task]
pub async fn sta_lease_log(stack: Stack<'static>) {
    loop {
        stack.wait_config_up().await;
        info!("{:?}", stack.config_v4());
        stack.wait_config_down().await;
    }
}
