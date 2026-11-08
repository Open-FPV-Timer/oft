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

## Design rules

- Timestamps are taken on the Node and mapped to the Timer clock; network latency never
  touches a lap time.
- `oft-core` has no I/O and no allocation. Hosts feed it decoded messages and their own
  SYNC/ping times.
- Everything above the radio is tested against `oft-sim` before it meets hardware. Real
  recordings replace synthetic scenes as they are captured.

GPL-3.0-or-later.
