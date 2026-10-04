# Handoff: esp_twin Wi-Fi repeater

Paste this (or tell Claude to read it) at the start of a new session on another machine.

## Goal
ESP32-S3-DevKitC-1 Wi-Fi repeater (no_std, esp-hal 1.x, esp-radio 1.0.0-beta.1, esp-rtos, Embassy,
embassy-net 0.9.1, smoltcp 0.14). STA joins the home router, AP serves clients, traffic is NATed
between them. The user is learning embedded Rust: be direct, give code when asked, hint before
fixing (see CLAUDE.md). Always Read a file before claiming something is missing.

## Done (all in src/bin/main.rs)
- AP+STA bring-up (`Config::AccessPointStation`), both `embassy_net` stacks, both runners spawned
  via `net_task` (`pool_size = 2`).
- `connection` task: connect loop, select on STA disconnect / AP client events, `break` on both
  Ok and Err of `wait_for_disconnect_async` (avoids a hot loop).
- `dhcp_server` task on the AP stack using edge-dhcp 0.8 / edge-nal-embassy 0.9
  (192.168.2.1/24, DNS 1.1.1.1 + 8.8.8.8). Hardware-verified: phone got 192.168.2.50.
- DNS change compiles but is not flashed. It can only be tested after forwarding works.

## Next: forwarding / NAPT
Why not a bridge: the router only accepts frames from the ESP's STA MAC, so a router-style NAPT is
needed. Rewrite the Ethernet src MAC, IP src and port, then recompute the IPv4 and TCP/UDP
checksums. Keep a table keyed on the reply direction, `(proto, translated_port) -> (client_ip,
client_port)`, with expiry. Handle ICMP and DNS separately. No existing no_std NAT crate was found.
Building blocks: `smoltcp::wire`, `embassy-net-driver`, `embassy-net-driver-channel`, `heapless`.

### Design discussed (agreed so far)
- `embassy_net::new` takes the `Interface` by value, so there is exactly one owner. If a NAT task
  owned the AP interface, `ap_stack` (DHCP server, 192.168.2.1) would stop working.
- So write a wrapper `struct NatDriver<D: Driver> { inner: D, /* channel to NAT task */ }` that
  implements `embassy_net_driver::Driver` by delegating to `inner`, but peeks at incoming frames.
  Pass `NatDriver(wifi_ap_device)` to `embassy_net::new`.
- `Driver::receive(&mut self, cx)` returns `(RxToken, TxToken)`, not a frame. The frame is only
  visible inside `RxToken::consume(self, |frame: &mut [u8]| ...)`. Parse it there with
  `smoltcp::wire::{EthernetFrame, Ipv4Packet}`.
- The frame borrow does not outlive the closure, so frames meant for NAT must be copied (e.g. into
  an `embassy_sync` channel / pool of fixed buffers) for the NAT task.

### Open questions (the user was working on these)
1. Rule for "keep for the stack" vs "divert to NAT", using the IPv4 header only. Cases: DHCP
   (UDP dst port 67 / broadcast), ping to 192.168.2.1, TCP to 8.8.8.8. Likely: dst is the ESP's own
   AP IP or broadcast/multicast -> stack, otherwise -> NAT.
2. Where do the diverted bytes get copied so another task can pick them up?
3. STA side needs the same treatment (replies from the router to translated ports go to NAT, the
   rest to the ESP's own stack). The user has not answered whether they want an embassy-net
   userspace relay or real NAPT below embassy-net; the wrapper-driver approach is NAPT below it.

## Other TODOs
- Use the router's DNS from the STA lease (`sta_stack.wait_config_up()` / `config_v4()`) instead of
  the hard-coded 1.1.1.1 / 8.8.8.8 (needs `sta_stack` passed into `dhcp_server`).
- Enable edge-dhcp's defmt feature and run with `DEFMT_LOG=debug` to see "IPv4: UP" / DHCP logs.
- `sta_stack` unused warning (goes away once forwarding uses it).
- `.unwrap()` at the end of `dhcp_server` panics and the panic handler loops. Decide if that is OK.
- Before committing: `cargo clippy` (warnings = errors) and `cargo fmt`. The repo has no commits yet.
- Verify every change on hardware with `cargo run` (probe-rs).
- SSID / PASSWORD / AP_SSID are build-time env vars (`.cargo/config.toml` `[env]`). Never
  hard-code or commit real values.
- Docs: use `ctx7` for library docs (user's global rule).

## Full transcript of the earlier session
/home/tmk/.claude/projects/-home-tmk-projects-rust-embedded-esp-twin/18d695fc-2d13-4b60-aa76-1408c0b35660.jsonl
(local to the desktop; copy it over if you want the raw history)
