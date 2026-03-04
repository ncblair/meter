use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use ringbuf::{HeapRb, traits::Consumer, traits::Producer, traits::Split};

const DEFAULT_DEVICE_NAME: &str = "music_out";
const ATTACK_MS: f32 = 1.0;
const RELEASE_MS: f32 = 200.0;
const UI_FPS: u64 = 30;
const DB_MIN: f32 = -60.0;
const DB_MAX: f32 = 12.0;
const METER_SEGMENTS: usize = 24;

#[derive(Clone, Copy, Default)]
struct Stereo {
    l: f32,
    r: f32,
}

#[derive(Clone)]
struct AppConfig {
    input_device_name: String,
    passthrough: bool,
}

fn pack_stereo(s: Stereo) -> u64 {
    let lb = s.l.to_bits() as u64;
    let rb = s.r.to_bits() as u64;
    (lb << 32) | rb
}

fn unpack_stereo(v: u64) -> Stereo {
    let l = f32::from_bits((v >> 32) as u32);
    let r = f32::from_bits(v as u32);
    Stereo { l, r }
}

fn coeff_from_ms(ms: f32, sample_rate: f32) -> f32 {
    let tau_s = ms * 0.001;
    (-1.0 / (tau_s * sample_rate)).exp()
}

fn parse_args() -> Result<AppConfig> {
    let mut input_device_name = DEFAULT_DEVICE_NAME.to_string();
    let mut passthrough = false;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list-devices" || a == "-l") {
        list_input_devices()?;
        std::process::exit(0);
    }

    for arg in &args {
        if arg == "--passthrough" || arg == "-p" {
            passthrough = true;
        } else if !arg.starts_with('-') {
            input_device_name = arg.clone();
        }
    }

    Ok(AppConfig {
        input_device_name,
        passthrough,
    })
}

fn main() -> Result<()> {
    let cfg = parse_args()?;
    let meter = Arc::new(AtomicU64::new(pack_stereo(Stereo::default())));

    let (input_stream, output_stream) = build_audio(&cfg, Arc::clone(&meter))?;
    input_stream
        .play()
        .context("failed to start audio input stream")?;
    if let Some(stream) = &output_stream {
        stream
            .play()
            .context("failed to start audio output stream")?;
    }

    let mut terminal = ratatui::init();
    let run_result = run_ui(&mut terminal, &cfg, &meter);
    ratatui::restore();

    run_result
}

fn list_input_devices() -> Result<()> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .context("failed to enumerate input devices")?;

    for device in devices {
        println!(
            "{}",
            device.name().unwrap_or_else(|_| "<unknown>".to_string())
        );
    }

    Ok(())
}

fn build_audio(
    cfg: &AppConfig,
    meter: Arc<AtomicU64>,
) -> Result<(cpal::Stream, Option<cpal::Stream>)> {
    let host = cpal::default_host();
    let mut devices = host
        .input_devices()
        .context("failed to enumerate input devices")?;

    let input_device = devices
        .find(|d| {
            d.name()
                .map(|n| n == cfg.input_device_name)
                .unwrap_or(false)
        })
        .with_context(|| format!("input device '{}' not found", cfg.input_device_name))?;

    let supported_input = input_device
        .default_input_config()
        .context("failed to read default input config for selected device")?;

    let sample_rate = supported_input.sample_rate().0 as f32;
    let channels_in = supported_input.channels() as usize;
    let attack_coeff = coeff_from_ms(ATTACK_MS, sample_rate);
    let release_coeff = coeff_from_ms(RELEASE_MS, sample_rate);
    let input_config: StreamConfig = supported_input.config();

    let mut maybe_output_stream = None;

    let input_stream = if cfg.passthrough {
        let output_device = host
            .default_output_device()
            .context("no default output device available")?;

        let supported_output = output_device
            .default_output_config()
            .context("failed to read default output config")?;

        let channels_out = supported_output.channels() as usize;
        let rb_capacity = (sample_rate as usize * 2).max(4096);
        let rb = HeapRb::<f32>::new(rb_capacity * 2);
        let (mut producer, mut consumer) = rb.split();

        let output_config: StreamConfig = supported_output.config();

        let out_err_fn = |err| {
            eprintln!("audio output stream error: {err}");
        };

        let output_stream = match supported_output.sample_format() {
            SampleFormat::F32 => output_device.build_output_stream(
                &output_config,
                move |data: &mut [f32], _| {
                    write_output_f32(data, channels_out, &mut consumer);
                },
                out_err_fn,
                None,
            ),
            SampleFormat::I16 => output_device.build_output_stream(
                &output_config,
                move |data: &mut [i16], _| {
                    write_output_i16(data, channels_out, &mut consumer);
                },
                out_err_fn,
                None,
            ),
            SampleFormat::U16 => output_device.build_output_stream(
                &output_config,
                move |data: &mut [u16], _| {
                    write_output_u16(data, channels_out, &mut consumer);
                },
                out_err_fn,
                None,
            ),
            other => {
                return Err(anyhow!("unsupported output sample format: {other:?}"));
            }
        }
        .context("failed to build output stream")?;

        maybe_output_stream = Some(output_stream);

        let in_err_fn = |err| {
            eprintln!("audio input stream error: {err}");
        };

        match supported_input.sample_format() {
            SampleFormat::F32 => {
                let mut state = Stereo::default();
                input_device.build_input_stream(
                    &input_config,
                    move |data: &[f32], _| {
                        process_audio_f32(
                            data,
                            channels_in,
                            attack_coeff,
                            release_coeff,
                            &mut state,
                            &meter,
                            |l, r| {
                                let _ = producer.try_push(l);
                                let _ = producer.try_push(r);
                            },
                        );
                    },
                    in_err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let mut state = Stereo::default();
                input_device.build_input_stream(
                    &input_config,
                    move |data: &[i16], _| {
                        process_audio_i16(
                            data,
                            channels_in,
                            attack_coeff,
                            release_coeff,
                            &mut state,
                            &meter,
                            |l, r| {
                                let _ = producer.try_push(l);
                                let _ = producer.try_push(r);
                            },
                        );
                    },
                    in_err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut state = Stereo::default();
                input_device.build_input_stream(
                    &input_config,
                    move |data: &[u16], _| {
                        process_audio_u16(
                            data,
                            channels_in,
                            attack_coeff,
                            release_coeff,
                            &mut state,
                            &meter,
                            |l, r| {
                                let _ = producer.try_push(l);
                                let _ = producer.try_push(r);
                            },
                        );
                    },
                    in_err_fn,
                    None,
                )
            }
            other => {
                return Err(anyhow!("unsupported input sample format: {other:?}"));
            }
        }
    } else {
        let in_err_fn = |err| {
            eprintln!("audio input stream error: {err}");
        };

        match supported_input.sample_format() {
            SampleFormat::F32 => {
                let mut state = Stereo::default();
                input_device.build_input_stream(
                    &input_config,
                    move |data: &[f32], _| {
                        process_audio_f32(
                            data,
                            channels_in,
                            attack_coeff,
                            release_coeff,
                            &mut state,
                            &meter,
                            |_, _| {},
                        );
                    },
                    in_err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let mut state = Stereo::default();
                input_device.build_input_stream(
                    &input_config,
                    move |data: &[i16], _| {
                        process_audio_i16(
                            data,
                            channels_in,
                            attack_coeff,
                            release_coeff,
                            &mut state,
                            &meter,
                            |_, _| {},
                        );
                    },
                    in_err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut state = Stereo::default();
                input_device.build_input_stream(
                    &input_config,
                    move |data: &[u16], _| {
                        process_audio_u16(
                            data,
                            channels_in,
                            attack_coeff,
                            release_coeff,
                            &mut state,
                            &meter,
                            |_, _| {},
                        );
                    },
                    in_err_fn,
                    None,
                )
            }
            other => {
                return Err(anyhow!("unsupported input sample format: {other:?}"));
            }
        }
    }
    .context("failed to build input stream")?;

    Ok((input_stream, maybe_output_stream))
}

fn apply_ballistics(input: f32, prev: f32, attack_coeff: f32, release_coeff: f32) -> f32 {
    let coeff = if input > prev {
        attack_coeff
    } else {
        release_coeff
    };
    input + coeff * (prev - input)
}

fn process_audio_f32(
    data: &[f32],
    channels: usize,
    attack_coeff: f32,
    release_coeff: f32,
    state: &mut Stereo,
    meter: &AtomicU64,
    mut push: impl FnMut(f32, f32),
) {
    if channels == 0 {
        return;
    }

    for frame in data.chunks_exact(channels) {
        let l = frame[0];
        let r = if channels > 1 { frame[1] } else { l };

        let l_in = l.abs();
        let r_in = r.abs();

        state.l = apply_ballistics(l_in, state.l, attack_coeff, release_coeff);
        state.r = apply_ballistics(r_in, state.r, attack_coeff, release_coeff);

        push(l, r);
    }

    meter.store(pack_stereo(*state), Ordering::Relaxed);
}

fn process_audio_i16(
    data: &[i16],
    channels: usize,
    attack_coeff: f32,
    release_coeff: f32,
    state: &mut Stereo,
    meter: &AtomicU64,
    mut push: impl FnMut(f32, f32),
) {
    if channels == 0 {
        return;
    }

    for frame in data.chunks_exact(channels) {
        let l = frame[0] as f32 / i16::MAX as f32;
        let r = if channels > 1 {
            frame[1] as f32 / i16::MAX as f32
        } else {
            l
        };

        let l_in = l.abs();
        let r_in = r.abs();

        state.l = apply_ballistics(l_in, state.l, attack_coeff, release_coeff);
        state.r = apply_ballistics(r_in, state.r, attack_coeff, release_coeff);

        push(l, r);
    }

    meter.store(pack_stereo(*state), Ordering::Relaxed);
}

fn process_audio_u16(
    data: &[u16],
    channels: usize,
    attack_coeff: f32,
    release_coeff: f32,
    state: &mut Stereo,
    meter: &AtomicU64,
    mut push: impl FnMut(f32, f32),
) {
    if channels == 0 {
        return;
    }

    for frame in data.chunks_exact(channels) {
        let l01 = frame[0] as f32 / u16::MAX as f32;
        let r01 = if channels > 1 {
            frame[1] as f32 / u16::MAX as f32
        } else {
            l01
        };

        let l = 2.0 * l01 - 1.0;
        let r = 2.0 * r01 - 1.0;

        state.l = apply_ballistics(l.abs(), state.l, attack_coeff, release_coeff);
        state.r = apply_ballistics(r.abs(), state.r, attack_coeff, release_coeff);

        push(l, r);
    }

    meter.store(pack_stereo(*state), Ordering::Relaxed);
}

fn write_output_f32(
    data: &mut [f32],
    channels_out: usize,
    consumer: &mut impl Consumer<Item = f32>,
) {
    if channels_out == 0 {
        return;
    }

    for frame in data.chunks_exact_mut(channels_out) {
        let l = consumer.try_pop().unwrap_or(0.0);
        let r = consumer.try_pop().unwrap_or(0.0);
        write_frame(frame, l, r);
    }
}

fn write_output_i16(
    data: &mut [i16],
    channels_out: usize,
    consumer: &mut impl Consumer<Item = f32>,
) {
    if channels_out == 0 {
        return;
    }

    for frame in data.chunks_exact_mut(channels_out) {
        let l = consumer.try_pop().unwrap_or(0.0);
        let r = consumer.try_pop().unwrap_or(0.0);
        write_frame_i16(frame, l, r);
    }
}

fn write_output_u16(
    data: &mut [u16],
    channels_out: usize,
    consumer: &mut impl Consumer<Item = f32>,
) {
    if channels_out == 0 {
        return;
    }

    for frame in data.chunks_exact_mut(channels_out) {
        let l = consumer.try_pop().unwrap_or(0.0);
        let r = consumer.try_pop().unwrap_or(0.0);
        write_frame_u16(frame, l, r);
    }
}

fn write_frame(frame: &mut [f32], l: f32, r: f32) {
    if frame.len() == 1 {
        frame[0] = 0.5 * (l + r);
        return;
    }

    frame[0] = l;
    frame[1] = r;
    let mut i = 2;
    while i < frame.len() {
        frame[i] = if i % 2 == 0 { l } else { r };
        i += 1;
    }
}

fn write_frame_i16(frame: &mut [i16], l: f32, r: f32) {
    let lc = (l.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
    let rc = (r.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;

    if frame.len() == 1 {
        frame[0] = ((0.5 * (l + r)).clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        return;
    }

    frame[0] = lc;
    frame[1] = rc;
    let mut i = 2;
    while i < frame.len() {
        frame[i] = if i % 2 == 0 { lc } else { rc };
        i += 1;
    }
}

fn write_frame_u16(frame: &mut [u16], l: f32, r: f32) {
    let lf = ((l.clamp(-1.0, 1.0) + 1.0) * 0.5 * u16::MAX as f32) as u16;
    let rf = ((r.clamp(-1.0, 1.0) + 1.0) * 0.5 * u16::MAX as f32) as u16;

    if frame.len() == 1 {
        frame[0] = (((0.5 * (l + r)).clamp(-1.0, 1.0) + 1.0) * 0.5 * u16::MAX as f32) as u16;
        return;
    }

    frame[0] = lf;
    frame[1] = rf;
    let mut i = 2;
    while i < frame.len() {
        frame[i] = if i % 2 == 0 { lf } else { rf };
        i += 1;
    }
}

fn run_ui(terminal: &mut DefaultTerminal, cfg: &AppConfig, meter: &AtomicU64) -> Result<()> {
    let tick = Duration::from_millis(1_000 / UI_FPS);
    let mut last_draw = Instant::now();

    loop {
        if event::poll(Duration::from_millis(10)).context("event poll failed")? {
            if let Event::Key(key) = event::read().context("event read failed")? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        _ => {}
                    }
                }
            }
        }

        if last_draw.elapsed() >= tick {
            let s = unpack_stereo(meter.load(Ordering::Relaxed));
            terminal
                .draw(|frame| render(frame, cfg, s))
                .context("terminal draw failed")?;
            last_draw = Instant::now();
        }
    }

    Ok(())
}

fn render(frame: &mut Frame, cfg: &AppConfig, stereo: Stereo) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(frame.area());

    let passthrough = if cfg.passthrough { "on" } else { "off" };
    let header = Paragraph::new(Line::from(format!(
        "In: {} | passthrough={} | q/esc quit | atk={}ms rel={}ms",
        cfg.input_device_name, passthrough, ATTACK_MS as u32, RELEASE_MS as u32
    )))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Stereo Peak Meter"),
    );

    let l_db = amp_to_dbfs(stereo.l);
    let r_db = amp_to_dbfs(stereo.r);

    let left = Paragraph::new(meter_line("L", l_db))
        .block(Block::default().borders(Borders::ALL).title("Left"));

    let right = Paragraph::new(meter_line("R", r_db))
        .block(Block::default().borders(Borders::ALL).title("Right"));

    frame.render_widget(header, chunks[0]);
    frame.render_widget(left, chunks[1]);
    frame.render_widget(right, chunks[2]);
}

fn amp_to_dbfs(amp: f32) -> f32 {
    let min = 1.0e-9;
    20.0 * amp.max(min).log10()
}

fn meter_line(label: &str, db: f32) -> Line<'static> {
    let mut spans = Vec::with_capacity(METER_SEGMENTS + 2);
    spans.push(Span::styled(
        format!("{label} {db:>6.1} dBFS "),
        Style::default().fg(Color::White),
    ));

    let lit = lit_segments_for_db(db);
    for i in 0..METER_SEGMENTS {
        let seg_db = DB_MIN + (i as f32 + 0.5) * ((DB_MAX - DB_MIN) / METER_SEGMENTS as f32);
        let color = band_color(seg_db);
        let style = if i < lit {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled("▮", style));
    }

    Line::from(spans)
}

fn lit_segments_for_db(db: f32) -> usize {
    if db <= DB_MIN {
        return 0;
    }
    if db >= DB_MAX {
        return METER_SEGMENTS;
    }

    let norm = (db - DB_MIN) / (DB_MAX - DB_MIN);
    (norm * METER_SEGMENTS as f32).floor() as usize
}

fn band_color(db: f32) -> Color {
    if db < -18.0 {
        Color::Green
    } else if db < -6.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}
