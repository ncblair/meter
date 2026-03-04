# tui_meter

Terminal stereo peak meter for system audio loopback capture.

## Install

```bash
./scripts/install.sh
```

Then:

```bash
meter-build
meter
```

## What this does

- Captures audio from a named input device (default: `music_out`)
- Tracks stereo peak envelopes with ballistics:
  - Attack: `1 ms`
  - Release: `200 ms`
- Renders left/right segmented LED-style meters in a TUI (tmux-friendly)
- Meter scale is linear from `-60 dBFS` to `+12 dBFS` across 24 segments
- Color bands: green `< -18 dB`, yellow `-18 to -6 dB`, red `>= -6 dB`
- Adds a per-channel scrolling oscilloscope to the right of each meter
- Optional in-app passthrough to the current default output device

## Setup (macOS + Rogue Amoeba Loopback)

1. In Loopback, create virtual device `music_out`.
2. Add the system audio source(s) you want to meter (or route app outputs into it).
3. Set your system/app output path so audio is flowing through `music_out`.
4. Ensure Terminal/iTerm has microphone/input permission in macOS Privacy settings.
5. If you use `--passthrough`, you do not need a Loopback monitor path.

## Build and run

```bash
cargo run --release
```

Command shortcuts:

```bash
meter-build   # build/update release binary
meter         # run meter with --passthrough
```

Run with in-app passthrough:

```bash
cargo run --release -- --passthrough
```

List available input devices:

```bash
cargo run --release -- --list-devices
```

If you want a different input device name:

```bash
cargo run --release -- "My Device Name"
```

Or with passthrough and explicit input:

```bash
cargo run --release -- "My Device Name" --passthrough
```

Press `q` or `Esc` to quit.

## Realtime notes

- No locks in the audio callback.
- No heap allocation in the callback.
- Ballistic coefficients are precomputed once (no `exp` in hot path).
- Stereo values are passed via a single `AtomicU64` packing both `f32` channels to avoid tearing.
- Passthrough uses a lock-free ring buffer between input and output callbacks.
- Scope data uses fixed-time min/max bins pushed over a lock-free queue, so scope behavior is independent of device block size.
