# Handoff: esp_twin Wi-Fi repeater

Paste this (or tell Claude to read it) at the start of a new session on another machine.

## Goal
ESP32-S3-DevKitC-1 Wi-Fi repeater (no_std, esp-hal 1.x, esp-radio 1.0.0-beta.1, esp-rtos, Embassy,
embassy-net 0.9.1, smoltcp 0.14). STA joins the home router, AP serves clients, traffic is NATed
between them. The user is learning embedded Rust: be direct, give code only when asked directly,
hint before fixing (see CLAUDE.md). Always Read a file before claiming something is missing, and
re-Read before editing: the user edits the file between turns (an earlier session overwrote their
in-progress function by editing from a stale read).

## Done (all in src/bin/main.rs)
- AP+STA bring-up (`Config::AccessPointStation`), both `embassy_net` stacks, both runners spawned
  via `net_task` (`pool_size = 2`).
- `connection` task: connect loop, select on STA disconnect / AP client events, `break` on both
  Ok and Err of `wait_for_disconnect_async` (avoids a hot loop).
- `dhcp_server` task on the AP stack using edge-dhcp 0.8 / edge-nal-embassy 0.9
  (192.168.2.1/24, DNS 1.1.1.1 + 8.8.8.8). Hardware-verified: phone got 192.168.2.50.
- DNS change compiles but is not flashed. It can only be tested after forwarding works.
- `const AP_IP: Ipv4Address` (192.168.2.1) now used for the AP static config and the DHCP server.
  `core::net::Ipv4Addr` and `smoltcp::wire::Ipv4Address` are the same type here.
- `enum Verdict { Stack, Nat }` and `fn classify_packet(&[u8]) -> Verdict` written and compiling
  (not yet called from anywhere, so there are dead-code warnings). Rule: unparsable frame, non-IPv4
  (ARP etc.), dst == AP_IP, broadcast, multicast or unspecified -> `Stack`; any other IPv4 dst ->
  `Nat`. Never panics (no `.unwrap()` in the RX path; the panic handler is `loop {}`).

## Next: forwarding / NAPT
Why not a bridge: the router only accepts frames from the ESP's STA MAC, so a router-style NAPT is
needed. Rewrite the Ethernet src MAC, IP src and port, then recompute the IPv4 and TCP/UDP
checksums. Handle ICMP and DNS separately. No existing no_std NAT crate was found.
Building blocks: `smoltcp::wire`, `embassy-net-driver`, `embassy-net-driver-channel`, `heapless`.

### Design (agreed)
- `embassy_net::new` takes the `Interface` by value, so there is exactly one owner. If a NAT task
  owned the AP interface, `ap_stack` (DHCP server, 192.168.2.1) would stop working.
- So write a wrapper `struct NatDriver<D: Driver> { inner: D }` that implements
  `embassy_net_driver::Driver` by delegating to `inner`, but peeks at incoming frames. Pass
  `NatDriver(wifi_ap_device)` to `embassy_net::new`.
- `Driver::receive(&mut self, cx)` returns `(RxToken, TxToken)`. The frame is only visible inside
  `RxToken::consume(self, |frame: &mut [u8]| ...)`, which is synchronous (no `.await`). Call
  `classify_packet` there.
- The frame borrow does not outlive the closure, so `Nat` frames are copied into a static channel:
  `static FRAMES: Channel<CriticalSectionRawMutex, Frame, N> = Channel::new();` (`Channel::new` is
  const; `try_send`/`receive` take `&self`, interior mutability via the RawMutex; critical-section
  mutex because the S3 has two cores). `Frame { data: [u8; 1514], len: usize }` (fixed size because
  a static channel needs a uniform item type; `len` = valid bytes). Consumer is a NAT task using
  `receive().await`.
- Producer must use `try_send` and drop on full (never block in the stack's RX path; IP/TCP
  tolerate loss). Decide: drop silently, log, or count. Oversize input (> 1514): drop or truncate.
  Mind stack usage when building a ~1.5 KB `Frame` by value.
- NAT table (heapless map): key `(proto, translated_port)`, value
  `{ client_mac, client_ip, client_port, last_seen }`. `client_mac` is needed for the reply's
  Ethernet dst and so the `connection` task can remove all entries for a client when the AP
  disconnect event (which carries a MAC) fires. Also expire by `last_seen` as a backstop.

### Immediate next step (user is doing this)
Add `embassy-sync = "0.8"` to Cargo.toml (not currently a direct dependency; 0.8.0 is what
embassy-net / esp-rtos / esp-hal use, there are also 0.6.2 and 0.7.2 in the tree), then write
`Frame`, the static channel, and `divert(packet: &[u8])` (copy + `try_send`). After that: the
`NatDriver` wrapper calling `classify_packet` + `divert`, then the NAT task.

### Open questions
1. STA side needs the same treatment (replies from the router to translated ports go to NAT, the
   rest to the ESP's own stack). The user has not answered whether they want an embassy-net
   userspace relay or real NAPT below embassy-net; the wrapper-driver approach is NAPT below it.
2. Which RX token / TX path the NAT task uses to inject rewritten frames out the other interface.

## Other TODOs
- Use the router's DNS from the STA lease (`sta_stack.wait_config_up()` / `config_v4()`) instead of
  the hard-coded 1.1.1.1 / 8.8.8.8 (needs `sta_stack` passed into `dhcp_server`).
- Enable edge-dhcp's defmt feature and run with `DEFMT_LOG=debug` to see "IPv4: UP" / DHCP logs.
- `sta_stack` unused warning (goes away once forwarding uses it).
- `.unwrap()` at the end of `dhcp_server` panics and the panic handler loops. Decide if that is OK.
- Before committing: `cargo clippy` (warnings = errors) and `cargo fmt`. `main.rs` has uncommitted
  changes.
- Verify every change on hardware with `cargo run` (probe-rs).
- SSID / PASSWORD / AP_SSID are build-time env vars (`.cargo/config.toml` `[env]`). Never
  hard-code or commit real values.
- Docs: use `ctx7` for library docs (user's global rule).

## Full transcript of the earlier session
/home/tmk/.claude/projects/-home-tmk-projects-rust-embedded-esp-twin/18d695fc-2d13-4b60-aa76-1408c0b35660.jsonl
(local to the desktop; copy it over if you want the raw history)
