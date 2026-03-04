# meter

`meter` is a terminal audio meter + oscilloscope for macOS loopback devices.

It is designed for tmux panes and live monitoring while you work.

## Features

- Stereo peak meter with ballistic envelope
  - Attack: 1 ms
  - Release: 200 ms
- LED-style meter scale
  - Linear from -60 dBFS to +12 dBFS
  - 24 segments
  - Green / Yellow / Red threshold bands
- Per-channel scrolling oscilloscope (min/max bucketed)
- Optional passthrough to your default output (`--passthrough`)
- Realtime-safe audio callbacks (no locks/allocations in the hot path)

## Requirements

- macOS
- Rust toolchain (`cargo`)
- A loopback input device (for example, Loopback or BlackHole)

## Quick Start

1. Clone this repo.
2. Install command wrappers:

```bash
./scripts/install.sh
```

3. Build once:

```bash
meter-build
```

4. Run:

```bash
meter
```

## Usage

```bash
meter [input-device-name] [--passthrough]
```

Examples:

```bash
meter
meter music_out
meter music_out --passthrough
```

List input devices:

```bash
cargo run --release -- --list-devices
```

Quit with `q` or `Esc`.

## Notes

- `meter` wrapper does not auto-build. Run `meter-build` when code changes.
- If your shell cannot find `meter`, reload your shell config (`source ~/.zshrc`) or open a new terminal.
