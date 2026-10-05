# Handoff: esp_twin Wi-Fi repeater

Paste this (or tell Claude to read it) at the start of a new session on another machine.

## Goal
ESP32-S3-DevKitC-1 Wi-Fi repeater (no_std, esp-hal 1.x, esp-radio 1.0.0-beta.1, esp-rtos, Embassy,
embassy-net 0.9.1, smoltcp 0.14). STA joins the home router, AP serves clients, traffic is NATed
between them. The user is learning embedded Rust: be direct, give code only when asked directly,
hint before fixing (see CLAUDE.md). Always Read a file before claiming something is missing, and
re-Read before editing: the user edits the file between turns (an earlier session overwrote their
in-progress function by editing from a stale read). Do not state a cause as fact before checking
the code (a past session called repeated log lines "TCP retransmits" when it was a missing
`receive_done()`).

## Done (all in src/bin/main.rs, hardware-verified unless noted)
- AP+STA bring-up (`Config::AccessPointStation`), both `embassy_net` stacks, runners spawned.
- `connection` task: connect loop, select on STA disconnect / AP client events.
- `dhcp_server` task on the AP stack (edge-dhcp 0.8 / edge-nal-embassy 0.9, 192.168.2.1/24).
  Phone and PC clients get 192.168.2.x leases (phone got .50).
- **DHCP DNS option is disabled on purpose.** `server_options.dns = &dns;` is commented out
  (marked `TEMP bisect`). With it enabled the phone sent Discovers, never got a usable reply, and
  dropped with `AuthenticationLeave` in a loop. Root cause not found yet (`edge-dhcp` server.rs
  passes `self.dns` straight into the reply builder). Re-enable only when investigating; the
  plan is to use the router's DNS from the STA lease instead.
- `const AP_IP` (192.168.2.1); `enum Verdict { Stack, Nat }`; `classify_packet(&[u8])`.
- **NAT RX plumbing works end to end:**
  - `ConstStaticCell<[Frame; 8]>` backing store, `zerocopy_channel::Channel<'static,
    CriticalSectionRawMutex, Frame>` via `mk_static!`, `.split()` into `send_half` /
    `receive_half`. `Frame { data: [u8; 1514], len: usize }`.
  - `divert(&mut Sender<'static, ..>, &[u8])`: drops (and bumps the `NAT_DROPS` `AtomicU32`) on
    oversize or full channel; otherwise copies into the slot with `try_send` / `send_done`.
  - `NatDriver<D: Driver> { inner, to_nat: Sender<'static, ..> }` wraps the AP `Interface` and is
    passed to `embassy_net::new`; all `Driver` methods delegate except `receive`, which returns a
    `NatRxToken<'a, R> { inner, to_nat: &'a mut Sender<'static, ..> }`. Its `consume` runs
    `classify_packet`: `Stack` -> `f(frame)`, `Nat` -> `divert` then `f(&mut [])`.
    (Do not use `&'a mut Sender<'a, ..>`: that forces the borrow to be `'static`.)
  - `nat_net_task` (concrete `Runner<'static, NatDriver<Interface>>`) runs the AP stack;
    `net_task` runs STA. Tasks cannot be generic.
  - `nat_log_task` receives frames, calls `log_frame` (TCP: addrs/ports/syn/ack, UDP: addrs/ports,
    other: protocol), then `rx.receive_done()`. `main`'s idle loop logs `NAT_DROPS` every 5 s.
- Last run logged phone traffic `192.168.2.50 -> 149.154.167.91` (74-byte frames). That run's log
  was affected by the missing `receive_done()` (since fixed, not yet re-flashed): re-run and
  check the new per-protocol log lines, and whether `Nat drops` stays 0.

## Next: forwarding / NAPT
Why not a bridge: the router only accepts frames from the ESP's STA MAC, so a router-style NAPT is
needed. Rewrite Ethernet src MAC, IP src and port, then recompute IPv4 and TCP/UDP checksums.
Handle ICMP and DNS separately. No existing no_std NAT crate was found. Building blocks:
`smoltcp::wire`, `embassy-net-driver` (re-exported as `embassy_net::driver`), `heapless`.

### Design (agreed so far)
- NAT table (heapless map): key `(proto, translated_port)`, value
  `{ client_mac, client_ip, client_port, last_seen }`. `client_mac` is needed for the reply's
  Ethernet dst and so the `connection` task can remove entries when the AP disconnect event
  (which carries a MAC) fires. Expire by `last_seen` as a backstop. Phones use randomized MACs.
- Plan, small hardware-tested steps: (1) outbound only: rewrite one client UDP DNS query and send
  it out STA; (2) inbound reply path with STA-side classification; (3) TCP, then ICMP.

### Open questions
1. How does the NAT task get a rewritten frame out the STA interface? The STA `Interface` is owned
   by `sta_stack`'s runner. Likely a second wrapper driver on STA whose `transmit` side drains a
   second (NAT -> STA) zerocopy channel. Open: how does the runner get woken when the NAT task has
   a frame ready? (Look at the `cx: &mut Context` argument of `Driver::receive/transmit`: register
   its waker, e.g. with an `embassy_sync::waitqueue::WakerRegistration`.)
2. STA side RX also needs classification: replies from the router to translated ports go to NAT,
   everything else to the ESP's own stack. A zerocopy channel is single-producer, so the STA side
   needs its own.
3. Who picks the translated source port, and how are collisions handled?

## Other TODOs
- **Bug:** `classify_packet` only treats `255.255.255.255` as broadcast. Directed broadcasts such as
  `192.168.2.255` are classified `Nat`. Treat any dst inside `192.168.2.0/24` (use `Ipv4Cidr`
  `contains_addr` / `broadcast`) as `Stack`.
- STA lease: log when `sta_stack` comes up (`wait_config_up()` / `config_v4()`). An earlier log
  showed `IPv4: DOWN`; forwarding needs a router lease first. Then use the router's DNS in the
  DHCP server (needs `sta_stack` passed into `dhcp_server`).
- Enable edge-dhcp's defmt feature and run with `DEFMT_LOG=debug` to see DHCP server logs.
- Unused-warnings go away as forwarding uses `sta_stack`; `Verdict` still needs a `defmt::Format`
  derive if you want to log it.
- `.unwrap()` at the end of `dhcp_server` panics and the panic handler loops. Decide if that is OK.
- Test clients: the phone drops Wi-Fi without internet (turn off mobile data / auto-switch). The
  PC's NetworkManager profile `ESP_WIFI` had a stale static `192.168.4.2`
  (`nmcli connection modify ESP_WIFI ipv4.method auto`); the PC is also on wired
  (192.168.1.x), so disable that interface to test internet through the ESP.
- Before committing: `cargo clippy` (warnings = errors) and `cargo fmt`.
- Verify every change on hardware with `cargo run` (probe-rs).
- SSID / PASSWORD / AP_SSID are build-time env vars (`.cargo/config.toml` `[env]`). Never
  hard-code or commit real values.
- Docs: use `ctx7` for library docs (user's global rule). Local crate sources:
  `~/.cargo/registry/src/*/` (e.g. `embassy-net-driver-0.2.0`, `edge-dhcp-0.8*`).
