use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

const DEFAULT_DEVICE_NAME: &str = "music_out";
const ATTACK_MS: f32 = 1.0;
const RELEASE_MS: f32 = 200.0;
const UI_FPS: u64 = 30;
const DB_MIN: f32 = -60.0;
const DB_MAX: f32 = 12.0;
const METER_SEGMENTS: usize = 24;
const SCOPE_POINTS_PER_SEC: u32 = 240;
const SCOPE_QUEUE_CAPACITY: usize = 32_768;
const SCOPE_HISTORY_CAPACITY: usize = 4096;

#[derive(Clone, Copy, Default)]
struct Stereo {
    l: f32,
    r: f32,
}

#[derive(Clone, Copy, Default)]
struct MinMax {
    min: f32,
    max: f32,
}

#[derive(Default)]
struct ScopeHistory {
    l: VecDeque<MinMax>,
    r: VecDeque<MinMax>,
}

#[derive(Clone)]
struct AppConfig {
    input_device_name: String,
    passthrough: bool,
}

struct ScopeBin {
    target_samples: u32,
    count: u32,
    lmin: f32,
    lmax: f32,
    rmin: f32,
    rmax: f32,
}

impl ScopeBin {
    fn new(target_samples: u32) -> Self {
        Self {
            target_samples,
            count: 0,
            lmin: 0.0,
            lmax: 0.0,
            rmin: 0.0,
            rmax: 0.0,
        }
    }

    fn push_sample(&mut self, l: f32, r: f32, producer: &mut HeapProd<u64>) {
        if self.count == 0 {
            self.lmin = l;
            self.lmax = l;
            self.rmin = r;
            self.rmax = r;
        } else {
            self.lmin = self.lmin.min(l);
            self.lmax = self.lmax.max(l);
            self.rmin = self.rmin.min(r);
            self.rmax = self.rmax.max(r);
        }

        self.count += 1;
        if self.count >= self.target_samples {
            let packed = pack_scope_point(self.lmin, self.lmax, self.rmin, self.rmax);
            let _ = producer.try_push(packed);
            self.count = 0;
        }
    }
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

fn f32_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

fn i16_to_f32(v: i16) -> f32 {
    (v as f32 / i16::MAX as f32).clamp(-1.0, 1.0)
}

fn pack_scope_point(lmin: f32, lmax: f32, rmin: f32, rmax: f32) -> u64 {
    let a = f32_to_i16(lmin) as u16 as u64;
    let b = f32_to_i16(lmax) as u16 as u64;
    let c = f32_to_i16(rmin) as u16 as u64;
    let d = f32_to_i16(rmax) as u16 as u64;
    (a << 48) | (b << 32) | (c << 16) | d
}

fn unpack_scope_point(v: u64) -> (MinMax, MinMax) {
    let lmin = i16_to_f32(((v >> 48) as u16) as i16);
    let lmax = i16_to_f32(((v >> 32) as u16) as i16);
    let rmin = i16_to_f32(((v >> 16) as u16) as i16);
    let rmax = i16_to_f32((v as u16) as i16);
    (
        MinMax {
            min: lmin,
            max: lmax,
        },
        MinMax {
            min: rmin,
            max: rmax,
        },
    )
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

    let (input_stream, output_stream, scope_cons) = build_audio(&cfg, Arc::clone(&meter))?;
    input_stream
        .play()
        .context("failed to start audio input stream")?;
    if let Some(stream) = &output_stream {
        stream
            .play()
            .context("failed to start audio output stream")?;
    }

    let mut terminal = ratatui::init();
    let run_result = run_ui(&mut terminal, &cfg, &meter, scope_cons);
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
) -> Result<(cpal::Stream, Option<cpal::Stream>, HeapCons<u64>)> {
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
    let scope_bucket_samples = ((sample_rate / SCOPE_POINTS_PER_SEC as f32).round() as u32).max(1);

    let scope_rb = HeapRb::<u64>::new(SCOPE_QUEUE_CAPACITY);
    let (mut scope_prod, scope_cons) = scope_rb.split();

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
                let mut scope_bin = ScopeBin::new(scope_bucket_samples);
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
                                scope_bin.push_sample(l, r, &mut scope_prod);
                            },
                        );
                    },
                    in_err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let mut state = Stereo::default();
                let mut scope_bin = ScopeBin::new(scope_bucket_samples);
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
                                scope_bin.push_sample(l, r, &mut scope_prod);
                            },
                        );
                    },
                    in_err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut state = Stereo::default();
                let mut scope_bin = ScopeBin::new(scope_bucket_samples);
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
                                scope_bin.push_sample(l, r, &mut scope_prod);
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
                let mut scope_bin = ScopeBin::new(scope_bucket_samples);
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
                                scope_bin.push_sample(l, r, &mut scope_prod);
                            },
                        );
                    },
                    in_err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let mut state = Stereo::default();
                let mut scope_bin = ScopeBin::new(scope_bucket_samples);
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
                                scope_bin.push_sample(l, r, &mut scope_prod);
                            },
                        );
                    },
                    in_err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut state = Stereo::default();
                let mut scope_bin = ScopeBin::new(scope_bucket_samples);
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
                                scope_bin.push_sample(l, r, &mut scope_prod);
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
    }
    .context("failed to build input stream")?;

    Ok((input_stream, maybe_output_stream, scope_cons))
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
    mut on_frame: impl FnMut(f32, f32),
) {
    if channels == 0 {
        return;
    }

    for frame in data.chunks_exact(channels) {
        let l = frame[0];
        let r = if channels > 1 { frame[1] } else { l };

        state.l = apply_ballistics(l.abs(), state.l, attack_coeff, release_coeff);
        state.r = apply_ballistics(r.abs(), state.r, attack_coeff, release_coeff);

        on_frame(l, r);
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
    mut on_frame: impl FnMut(f32, f32),
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

        state.l = apply_ballistics(l.abs(), state.l, attack_coeff, release_coeff);
        state.r = apply_ballistics(r.abs(), state.r, attack_coeff, release_coeff);

        on_frame(l, r);
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
    mut on_frame: impl FnMut(f32, f32),
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

        on_frame(l, r);
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

fn run_ui(
    terminal: &mut DefaultTerminal,
    cfg: &AppConfig,
    meter: &AtomicU64,
    mut scope_cons: HeapCons<u64>,
) -> Result<()> {
    let tick = Duration::from_millis(1_000 / UI_FPS);
    let mut last_draw = Instant::now();
    let mut scope = ScopeHistory::default();

    loop {
        drain_scope_queue(&mut scope_cons, &mut scope);

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
                .draw(|frame| render(frame, cfg, s, &scope))
                .context("terminal draw failed")?;
            last_draw = Instant::now();
        }
    }

    Ok(())
}

fn drain_scope_queue(scope_cons: &mut HeapCons<u64>, scope: &mut ScopeHistory) {
    while let Some(packed) = scope_cons.try_pop() {
        let (l, r) = unpack_scope_point(packed);

        scope.l.push_back(l);
        scope.r.push_back(r);

        if scope.l.len() > SCOPE_HISTORY_CAPACITY {
            let _ = scope.l.pop_front();
        }
        if scope.r.len() > SCOPE_HISTORY_CAPACITY {
            let _ = scope.r.pop_front();
        }
    }
}

fn render(frame: &mut Frame, _cfg: &AppConfig, stereo: Stereo, scope: &ScopeHistory) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(frame.area());

    let l_db = amp_to_dbfs(stereo.l);
    let r_db = amp_to_dbfs(stereo.r);

    render_channel(frame, chunks[0], "Left", "L", l_db, &scope.l, Color::Green);
    render_channel(frame, chunks[1], "Right", "R", r_db, &scope.r, Color::Green);
}

fn render_channel(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    label: &str,
    db: f32,
    trace: &VecDeque<MinMax>,
    scope_color: Color,
) {
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 8 || inner.height < 1 {
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(40), Constraint::Min(8)])
        .split(inner);

    let meter_y = cols[0]
        .y
        .saturating_add(cols[0].height.saturating_sub(1) / 2);
    let meter_rect = Rect {
        x: cols[0].x,
        y: meter_y,
        width: cols[0].width,
        height: 1,
    };

    frame.render_widget(Paragraph::new(meter_line(label, db)), meter_rect);

    let scope_lines = scope_lines(trace, cols[1].width as usize, cols[1].height as usize);
    let scope_paragraph = Paragraph::new(scope_lines).style(Style::default().fg(scope_color));
    frame.render_widget(scope_paragraph, cols[1]);
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

fn scope_lines(trace: &VecDeque<MinMax>, width: usize, height: usize) -> Vec<Line<'static>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let sub_w = width * 2;
    let sub_h = height * 4;
    let mut dots = vec![vec![false; sub_w]; sub_h];

    let center = sample_to_subrow(0.0, sub_h);
    for x in 0..sub_w {
        if x % 2 == 0 {
            dots[center][x] = true;
        }
    }

    let visible = width.min(trace.len());
    let start = trace.len().saturating_sub(visible);
    let x_offset = width - visible;

    for (i, mm) in trace.iter().skip(start).enumerate() {
        let x = (x_offset + i) * 2 + 1;
        let top = sample_to_subrow(mm.max, sub_h);
        let bottom = sample_to_subrow(mm.min, sub_h);
        let y0 = top.min(bottom);
        let y1 = top.max(bottom);

        for row in dots.iter_mut().take(y1 + 1).skip(y0) {
            row[x] = true;
        }

        if x > 0 {
            dots[top][x - 1] = true;
            dots[bottom][x - 1] = true;
        }
    }

    let mut lines = Vec::with_capacity(height);
    for cell_row in 0..height {
        let mut line = String::with_capacity(width);
        for cell_col in 0..width {
            let mut bits: u8 = 0;
            for dy in 0..4 {
                for dx in 0..2 {
                    if dots[cell_row * 4 + dy][cell_col * 2 + dx] {
                        bits |= braille_bit(dx, dy);
                    }
                }
            }
            line.push(char::from_u32(0x2800 + bits as u32).unwrap_or(' '));
        }
        lines.push(Line::from(line));
    }

    lines
}

fn sample_to_subrow(sample: f32, sub_height: usize) -> usize {
    if sub_height <= 1 {
        return 0;
    }

    let y = (1.0 - sample.clamp(-1.0, 1.0)) * 0.5 * (sub_height as f32 - 1.0);
    y.round() as usize
}

fn braille_bit(dx: usize, dy: usize) -> u8 {
    match (dx, dy) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (0, 3) => 0x40,
        (1, 3) => 0x80,
        _ => 0,
    }
}
