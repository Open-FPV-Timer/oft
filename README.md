# OFT — Open FPV Timer (Rust workspace)

Shared code for every OFT Timer host. The spec lives in the sibling `oft-spec` repo.

| Crate | `no_std` | What |
|---|---|---|
| `oft-nodelink` | ✅ | Node ↔ Timer link: COBS + CRC-16 frames, message types (spec §5) |
| `oft-core` | ✅ | `ClockMap` (Node clock → Timer clock), `Detector` (`edge_mid`, spec §8), `NodeHost` (one Node's Timer-side state) |
| `oft-sim` | std | Synthetic passes (`Scene`), a drifting `NodeClock`, and a `FakeNode` that speaks the link in memory |
| `oftd` | std | Linux Timer: serves the OFT Protocol (spec §6) over HTTP + WebSocket. `--fake` runs without hardware |

```sh
cargo test                                   # everything, no hardware needed
cargo test -p oft-sim --test detect print_margins -- --ignored --nocapture   # detection error table
cargo check -p oft-core -p oft-nodelink --target thumbv8m.main-none-eabihf   # RP2350 build check
```

```sh
cargo build -p oftd
target/debug/oftd --fake                     # simulated Node, serves on 0.0.0.0:7270
target/debug/oftd --serial /dev/ttyACM0      # real Node
```

`oftd` advertises itself over mDNS as `<id>._oft._tcp.local.` (spec §6.1); `--no-mdns` turns
that off, `--id` sets the id. `cargo test -p oftd --test api mdns -- --ignored` checks the
advertisement on a host with multicast.

## Design rules

- Timestamps are taken on the Node and mapped to the Timer clock; network latency never
  touches a lap time.
- `oft-core` has no I/O and no allocation. Hosts feed it decoded messages and their own
  SYNC/ping times.
- Everything above the radio is tested against `oft-sim` before it meets hardware. Real
  recordings replace synthetic scenes as they are captured.

## Acknowledgements

OFT started from reading other people's open work. These projects shaped it:

- **[FPVGateC5MK](https://github.com/LouisHitchcock/FPVGateC5MK)**,
  **[FPVGateC5RX](https://github.com/LouisHitchcock/FPVGateC5RX)** and
  **[FPVGate](https://github.com/LouisHitchcock/FPVGate)** by Louis Hitchcock. The main
  inspiration for this project. FPVGate showed that an ESP32-C5 can replace the
  hard-to-buy RX5808 as a lap-timing receiver, and FPVGateC5MK showed that one C5 can hop
  across a full heat of pilots fast enough to time them all. OFT's Node is built on that
  idea. FPVGate is licensed CC BY-NC-SA 4.0, so OFT takes the idea and its published
  measurements, not its code.
- **[widscan58](https://github.com/CapnBry/widscan58)** by CapnBry (GPL-3.0-or-later). The
  OFT Node firmware ([oft-node-c5](https://github.com/Open-FPV-Timer/oft-node-c5)) is a fork
  of it: the scanner, tuning, level measurement and web flasher are widscan58's, and OFT
  adds a mode that streams levels to a Timer.
- **[esp-sdr](https://github.com/ESPARGOS/esp-sdr)** and
  **[C5VRX](https://github.com/Twotoz/C5VRX)**, which widscan58 builds on. They found and
  published the way to capture raw radio samples on the ESP32-C5 that all of the above
  depend on.
- **[RotorHazard](https://github.com/RotorHazard/RotorHazard)**. The enter/exit threshold
  model, the manual calibration mode and much of what we know about RX5808 timing come
  from reading its node firmware and server. OFT's RotorHazard plugin
  ([rh-oft](https://github.com/Open-FPV-Timer/rh-oft)) runs inside it.

The [research notes](https://github.com/Open-FPV-Timer/oft-spec/blob/main/research-notes-2026-11.md)
record what we learned from each. If you like OFT, go and look at theirs.

GPL-3.0-or-later.
