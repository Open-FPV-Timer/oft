# Recorded flights

Real signal recorded from a Timer's WebSocket stream with `bench/record.mjs` (one JSON
message per line, gzip). Kept as ground truth for detector tests.

## whoop-2ft-gate-2026-11-12.jsonl.gz

First flight test of build B1. A whoop at 25 mW on Raceband 1 (5658 MHz), a 2 x 2 ft PVC
gate, Timer on the top bar with the Node facing the ground. One seat at 1050 samples/s,
8 min 20 s (Timer time 100 to 600 s), no lost samples. Signal values are tenths of a dB.

Settings: auto threshold, 22 dB above the floor to enter, 5 dB hysteresis, exit hold 80 ms,
minimum lap 1.5 s. The floor was about -54.7 dB, so the enter level was about -32.8 dB.

What the pilot flew, with the time of each signal peak (Timer seconds):

| Time | What | Raw peak, dB | Counted |
|---|---|---|---|
| 121.9, 126.1, 130.4 | 3 passes at normal pace | -24.5 to -25.6 | yes |
| 134.8, 141.4, 147.0 | 3 fast passes | -20.6 to -26.4 | yes |
| 154.3, 162.0, 170.4 | 3 slow passes, low and level in the gate | -27.1 to -30.9 | yes |
| 178.1, 185.8, 193.6 | 3 near misses (left, right and top, order not recorded) | -34.5 to -37.2 | no |
| 514.0, 519.5, 524.6, 529.9, 534.4 | 5 low passes at normal pace | -29.0 to -30.6 | yes |
| 548.9, 555.1, 560.1, 565.4, 570.5 | 5 skims just over the top bar | -31.9 to -35.2 | no |

The file holds the Timer's own 14 `crossing` messages for those passes. A correct detector
with these settings reports the same 14 and nothing for the 8 near misses. Two of the skims
(555.1 and 560.1) are above the enter level for 5 and 10 ms in the raw signal; only the
smoothing keeps them out. The gap between the weakest low pass and the strongest skim is
about 1 dB, so this recording is a hard case, not a comfortable one.

## whoop-race-track-2026-11-15.jsonl.gz

The same whoop and Timer on a RaceGOW practice track, two races (Timer time 5540 to 6000 s),
same settings as above. The track passes close by the gate about 1.6 s before going
through it, so most laps show two bumps: a fly-by of 20 to 22 dB over the floor, then the
real pass at 25 to 31 dB.

- Where the fly-by reaches the enter level it is counted as the lap and the real pass is
  then inside the minimum lap: laps at 5583.130, 5821.476 and 5905.130 s are 0.5 to 0.75 s
  early. With a pass window of 2500 ms they move to 5583.753, 5822.228 and 5905.627 s (the
  real passes) and every other lap is unchanged.
- The first pass of the second race (about 5805.3 s) peaks 19.8 dB over the floor and is
  not counted at 22 dB.

Replay either file through the detector with
`cargo run -p oftd --example replay -- <file.jsonl> [snr_enter] [hysteresis] [min_lap_ms] [exit_hold_ms] [from_s] [to_s] [pass_window_ms]`
(unpack it first).
