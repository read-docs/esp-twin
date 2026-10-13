use smoltcp::wire::{EthernetAddress, Ipv4Address, Ipv4Cidr};

pub const SSID: &str = env!("SSID");
pub const PASSWORD: &str = env!("PASSWORD");
pub const AP_SSID: &str = env!("AP_SSID");
pub const GATEWAY_MAC: &str = env!("GATEWAY_MAC");
pub const AP_IP: Ipv4Address = Ipv4Address::new(192, 168, 2, 1);
pub const AP_NET: Ipv4Cidr = Ipv4Cidr::new(AP_IP, 24);

pub fn parse_mac(s: &str) -> EthernetAddress {
    let mut bytes = [0u8; 6];
    let mut parts = s.split(':');
    for b in bytes.iter_mut() {
        *b = u8::from_str_radix(parts.next().expect("MAC too short"), 16).expect("bad MAC hex");
    }
    assert!(parts.next().is_none(), "MAC too long");
    EthernetAddress(bytes)
}
