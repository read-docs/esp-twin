#![no_std]

/// Leaks a value into a `static`, returning `&'static mut T`. Can only run once per call site.
#[macro_export]
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
        x
    }};
}

pub mod config;
pub mod debug;
pub mod dhcp;
pub mod driver;
pub mod frame;
pub mod nat;
pub mod tasks;
pub mod wifi;
