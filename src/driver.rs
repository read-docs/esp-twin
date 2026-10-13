//! `embassy_net` driver wrappers that intercept frames between the radios and the stacks.

use core::task::Context;

use defmt::info;
use embassy_net::driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::waitqueue::AtomicWaker;
use embassy_sync::zerocopy_channel::{Receiver, Sender};
use smoltcp::wire::EthernetAddress;

use crate::frame::Frame;
use crate::nat::{Verdict, classify_inbound, classify_packet, divert, forward_inbound};

pub static STA_WAKER: AtomicWaker = AtomicWaker::new();
pub static AP_WAKER: AtomicWaker = AtomicWaker::new();

pub struct StaDriver<D: Driver> {
    pub inner: D,
    pub from_nat: Receiver<'static, CriticalSectionRawMutex, Frame>,
    pub to_ap: Sender<'static, CriticalSectionRawMutex, Frame>,
    pub ap_mac: EthernetAddress,
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

pub struct StaRxToken<'a, R: RxToken> {
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

pub struct NatDriver<D: Driver> {
    pub inner: D,
    pub to_nat: Sender<'static, CriticalSectionRawMutex, Frame>,
    pub from_sta: Receiver<'static, CriticalSectionRawMutex, Frame>,
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

pub struct NatRxToken<'a, R: RxToken> {
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
