# OFT — Open FPV Timer (Rust workspace)

Shared code for every OFT Timer host. The spec lives in the sibling `oft-spec` repo.

| Crate | `no_std` | What |
|---|---|---|
| `oft-nodelink` | ✅ | Node ↔ Timer link: COBS + CRC-16 frames, message types (spec §5) |
| `oft-core` | ✅ | `ClockMap` (Node clock → Timer clock), `Detector` (`edge_mid`, spec §8), `NodeHost` (one Node's Timer-side state), `sched` and `Hosts` (several Nodes: which Node watches which seat, spec §4.1) |
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

`--fake` is the mock Timer for testing race software without hardware. It starts with one
seat; set up to 8 with `PUT /oft/v1/config` (or on its page, `http://localhost:7270/`) and
every seat gets its own simulated pilot. Seat 0 laps every `--fake-lap-s` seconds (default
8), each further seat `--fake-lap-spread` percent slower (default 4), and each lap varies
by up to `--fake-lap-jitter-s` (default 0.3). The laps are the same on every run. As with
a real Node, the seats share its sample rate: 1000 samples/s with one seat, 125 with eight.

With a real Node, `oftd` keeps its saved settings in `~/.config/oftd/settings.json`
(`--settings <file>` to choose, `--no-settings` for none; `--fake` keeps none unless given a
file). "Save as power-on settings" on the page writes the config in use there, and the
next start comes back with it.

Its id is `oft-` and four hex digits that stay the same on one machine (from
`/etc/machine-id`).

`oftd` advertises itself over mDNS as `<id>._oft._tcp.local.` (spec §6.1); `--no-mdns` turns
that off, `--id` sets the id. `cargo test -p oftd --test api mdns -- --ignored` checks the
advertisement on a host with multicast.

## What is protocol and what is not

`oftd` serves the OFT Protocol (spec §6) for race software. Its own page, and the things
that page does on this Timer (updating the Node's firmware, `/fw/node`), are not part of
the protocol: `/info` points to the page with `manage_url` and lists what it can do here
under `manage`. `/status` carries `problems` (`node_missing` when the Node is not
answering).

## One protocol, two implementations

`oftd` and the RP2350 firmware ([oft-timer-rp2350](../oft-timer-rp2350)) share `oft-core`
and `oft-nodelink` but build their HTTP replies in separate code. `bench/conformance.py`
keeps them in step: it asks a Timer the same questions and checks every reply against one
description of its shape. Run it against each kind after changing either:

```sh
bench/conformance.py --fake                          # builds and checks oftd --fake
bench/conformance.py http://192.168.1.50:7270        # any Timer; safe on one in use
```

## On a Raspberry Pi timer

`cross/build-pi.sh` builds `oftd` for a 64-bit Raspberry Pi in Docker. Copy the binary and
`dist/install-service.sh` to the Pi and run `sudo ./install-service.sh` there: it installs
`oftd` as a service on the Pi's UART (`/dev/ttyAMA0`), started at every boot. Run it again
to update.

On a RotorHazard-style board whose Node is an STM32 (a NuclearHazard, for one), tell `oftd`
which GPIOs hold the STM32's reset and BOOT0 lines and the Timer page can update the Node's
firmware (`/fw/node`), as the RP2350 Timer does for a C5:

```sh
sudo ./install-service.sh /dev/ttyAMA0 --stm32-reset-gpio 17 --stm32-boot0-gpio 27
```

For an ESP32-C5 Node wired to the Pi's UART (the C5's D6/D7) with its EN and BOOT pins on
two GPIOs, the same works with `--esp-en-gpio <n> --esp-boot-gpio <n>`. The protocol is the
`oft-esprom` crate, which the RP2350 Timer uses too. A C5 on the Pi's USB is not updated by
`oftd` yet: flash it over its own USB port.

The first time it replaces firmware that is not an OFT Node's, it saves what was on the chip
as `~/.config/oftd/node-backup-<hash>.bin`.

## Control: one writer at a time

Any number of clients can watch a Timer; one at a time may change it (spec §6.6). A client
claims control on its stream, and while a claim holds control every write without the
holder's key is refused with 423. The logic is `oft_core::control`, shared by `oftd` and
the RP2350 firmware. The Timer's page shows who holds control (Setup, "Control"), is
read-only while another client does, and can hand control to any client that has claimed
it. With no claims, anyone may write: `curl -X PUT .../config` still works on a bench.

This is a technical control, not security. It keeps well-behaved clients from undoing each
other's settings. Everything is plain HTTP, so someone on the same network who means harm
can read the holder's key off the wire and write with it, or pose as the Timer's page. The
Timer is meant for the local race network only: keep it off networks strangers can join.

## Level units

A Node says what its level numbers measure (`oft_nodelink::msg::Level`): tenths of a dB for
a Node that computes signal power (the C5), millivolts for one that reads a receiver's
signal-strength pin (the STM32 + RX5808 Node). Samples, the noise floor, thresholds, `peak`
and `snr` are all in that unit, and `oft-core` does not care which. Both Timers publish it
as `nodes[].level` in `/info` (`{"unit":"dB","scale":0.1}` or `{"unit":"mV","scale":1}`), and
`web/timer.html` labels everything from it. Clients must read it and not assume dB
(spec §6.4).

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
