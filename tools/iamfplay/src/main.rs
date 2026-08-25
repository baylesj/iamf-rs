//! Terminal IAMF player. Pre-decodes the stream to three renders —
//! stereo, binaural (HRTF), and a 7.1.4 "bed" used for channel meters —
//! then plays with an instant, sample-aligned A/B toggle between stereo
//! and binaural. Intended as a headphone demo of the decoder; use
//! `iamfdec` for offline rendering to arbitrary layouts.

mod demo;
mod mp4;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::stream::{MixSelection, StreamDecoder, StreamSettings};
use iamf_obu::descriptors::{self, Descriptor};
use iamf_obu::{ByteReader, Obu, ObuType};
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};

const BED_LAYOUT: SoundSystem = SoundSystem::J;
const BED_LABELS: [&str; 12] = [
    "L", "R", "C", "LFE", "Lss", "Rss", "Lrs", "Rrs", "Ltf", "Rtf", "Ltb", "Rtb",
];
/// Crossfade length for the stereo/binaural toggle, in output frames.
const FADE_FRAMES: u32 = 480;

/// Byte offset where the descriptor OBUs end and temporal units begin.
pub fn descriptor_split(data: &[u8]) -> Option<usize> {
    let mut reader = ByteReader::new(data);
    let mut end = 0;
    loop {
        let position = reader.position();
        let Ok(obu) = Obu::parse(&mut reader) else {
            return None;
        };
        match obu.header.obu_type {
            ObuType::SequenceHeader
            | ObuType::CodecConfig
            | ObuType::AudioElement
            | ObuType::MixPresentation => end = reader.position(),
            _ => return Some(position.max(end)),
        }
        if reader.position() >= data.len() {
            return Some(end);
        }
    }
}

struct Render {
    /// Interleaved f32 samples.
    pcm: Vec<f32>,
    channels: usize,
    rate: u32,
    mix_id: u32,
    realtime_factor: f64,
}

fn predecode(
    descriptors: &[u8],
    media: &[u8],
    layout: SoundSystem,
    mix: MixSelection,
) -> Result<Render, String> {
    let settings = StreamSettings {
        layout,
        sample_type: None,
        mix_selection: mix,
        ..StreamSettings::default()
    };
    let start = Instant::now();
    let mut decoder = StreamDecoder::new_from_descriptors(descriptors, settings, &DefaultFactory)
        .map_err(|e| format!("decoder init ({layout:?}): {e:?}"))?;
    let sample_bytes = decoder.sample_type().bytes_per_sample();
    let mut pcm = Vec::new();
    let mut push = |bytes: &[u8]| {
        if sample_bytes == 2 {
            pcm.extend(
                bytes
                    .chunks_exact(2)
                    .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0),
            );
        } else {
            pcm.extend(
                bytes
                    .chunks_exact(4)
                    .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2147483648.0),
            );
        }
    };
    for chunk in media.chunks(4096) {
        decoder
            .decode(chunk)
            .map_err(|e| format!("decode ({layout:?}): {e:?}"))?;
        while let Some(unit) = decoder
            .get_output_temporal_unit()
            .map_err(|e| format!("render ({layout:?}): {e:?}"))?
        {
            push(&unit);
        }
    }
    decoder.signal_end_of_decoding();
    while let Some(unit) = decoder
        .get_output_temporal_unit()
        .map_err(|e| format!("render ({layout:?}): {e:?}"))?
    {
        push(&unit);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let channels = decoder.num_output_channels();
    let rate = decoder.sample_rate();
    if pcm.is_empty() || channels == 0 {
        return Err(format!("stream produced no audio for {layout:?}"));
    }
    let duration = pcm.len() as f64 / channels as f64 / rate as f64;
    Ok(Render {
        pcm,
        channels,
        rate,
        mix_id: decoder.selected_mix().0,
        realtime_factor: duration / elapsed.max(1e-9),
    })
}

struct Renders {
    stereo: Render,
    binaural: Render,
    bed: Render,
}

fn predecode_all(descriptors: &[u8], media: &[u8], mix: MixSelection) -> Result<Renders, String> {
    let stereo = predecode(descriptors, media, SoundSystem::A, mix)?;
    // Pin the other renders to the mix the first selection resolved to, so
    // A/B toggling never switches mixes underneath the listener.
    let pinned = MixSelection::ById(stereo.mix_id);
    let (binaural, bed) = std::thread::scope(|scope| {
        let binaural = scope.spawn(|| predecode(descriptors, media, SoundSystem::Binaural, pinned));
        let bed = predecode(descriptors, media, BED_LAYOUT, pinned);
        (binaural.join().expect("binaural predecode panicked"), bed)
    });
    Ok(Renders {
        stereo,
        binaural: binaural?,
        bed: bed?,
    })
}

/// Lock-free controls shared with the audio callback.
struct Control {
    /// 0 = stereo, 1 = binaural.
    mode: AtomicU8,
    paused: AtomicBool,
    /// Playback position in source frames (published by the callback).
    pos: AtomicU64,
    /// Pending seek in source frames (consumed by the callback).
    seek: AtomicI64,
}

fn build_stream(
    device: &cpal::Device,
    renders: &Renders,
    control: Arc<Control>,
) -> Result<cpal::Stream, String> {
    let config = device
        .default_output_config()
        .map_err(|e| format!("no output config: {e}"))?;
    let sample_format = config.sample_format();
    let config: cpal::StreamConfig = config.into();
    match sample_format {
        cpal::SampleFormat::F32 => build_stream_typed::<f32>(device, &config, renders, control),
        cpal::SampleFormat::I16 => build_stream_typed::<i16>(device, &config, renders, control),
        cpal::SampleFormat::U16 => build_stream_typed::<u16>(device, &config, renders, control),
        other => Err(format!("unsupported device sample format {other:?}")),
    }
}

fn build_stream_typed<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    renders: &Renders,
    control: Arc<Control>,
) -> Result<cpal::Stream, String> {
    let out_channels = config.channels as usize;
    let step = f64::from(renders.stereo.rate) / f64::from(config.sample_rate.0);
    let buffers: [Arc<Vec<f32>>; 2] = [
        Arc::new(renders.stereo.pcm.clone()),
        Arc::new(renders.binaural.pcm.clone()),
    ];
    let total_frames = (buffers[0].len() / 2).min(buffers[1].len() / 2) as u64;
    let mut phase = 0.0f64;
    let mut current_mode = control.mode.load(Ordering::Relaxed);
    let mut fade: Option<(u8, u32)> = None;

    let sample_at = move |buf: &[f32], frame: u64, frac: f32, ch: usize| -> f32 {
        let i = (frame % total_frames) as usize * 2 + ch;
        let j = ((frame + 1) % total_frames) as usize * 2 + ch;
        buf[i] + (buf[j] - buf[i]) * frac
    };

    let callback = move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
        let paused = control.paused.load(Ordering::Relaxed);
        let seek = control.seek.swap(0, Ordering::Relaxed);
        if seek != 0 {
            let pos = phase as i64 + seek;
            phase = pos.rem_euclid(total_frames as i64) as f64;
        }
        let target_mode = control.mode.load(Ordering::Relaxed);
        for frame_out in data.chunks_exact_mut(out_channels) {
            if paused {
                frame_out.fill(T::from_sample(0.0));
                continue;
            }
            if target_mode != current_mode && fade.is_none() {
                fade = Some((current_mode, 0));
                current_mode = target_mode;
            }
            let frame = phase as u64;
            let frac = (phase - frame as f64) as f32;
            for (ch, slot) in frame_out.iter_mut().enumerate().take(2) {
                let mut value = sample_at(&buffers[current_mode as usize], frame, frac, ch);
                if let Some((from, progress)) = fade {
                    let mix = progress as f32 / FADE_FRAMES as f32;
                    let old = sample_at(&buffers[from as usize], frame, frac, ch);
                    value = old + (value - old) * mix;
                }
                *slot = T::from_sample(value);
            }
            for slot in frame_out.iter_mut().skip(2) {
                *slot = T::from_sample(0.0);
            }
            if let Some((from, progress)) = fade {
                fade = (progress + 1 < FADE_FRAMES).then_some((from, progress + 1));
            }
            phase += step;
            if phase >= total_frames as f64 {
                phase -= total_frames as f64;
            }
        }
        control.pos.store(phase as u64, Ordering::Relaxed);
    };
    device
        .build_output_stream(config, callback, |e| eprintln!("audio error: {e}"), None)
        .map_err(|e| format!("failed to open audio stream: {e}"))
}

/// Peak level per bed channel over the trailing window, as 0..=1.
fn bed_peaks(bed: &Render, pos_frames: u64, window: usize) -> Vec<f64> {
    let ch = bed.channels;
    let frames = bed.pcm.len() / ch;
    if frames == 0 {
        return vec![0.0; ch];
    }
    let end = (pos_frames as usize).min(frames);
    let start = end.saturating_sub(window);
    let mut peaks = vec![0.0f32; ch];
    for frame in start..end {
        for (c, peak) in peaks.iter_mut().enumerate() {
            *peak = peak.max(bed.pcm[frame * ch + c].abs());
        }
    }
    // Map to a dB meter with a -60 dB floor.
    peaks
        .into_iter()
        .map(|p| {
            let db = 20.0 * p.max(1e-6).log10();
            f64::from((db + 60.0).clamp(0.0, 60.0) / 60.0)
        })
        .collect()
}

struct MixInfo {
    ids: Vec<u32>,
    codec: String,
}

fn parse_mix_info(descriptors_bytes: &[u8]) -> MixInfo {
    let mut info = MixInfo {
        ids: Vec::new(),
        codec: "?".into(),
    };
    let mut reader = ByteReader::new(descriptors_bytes);
    while let Ok(obu) = Obu::parse(&mut reader) {
        if let Ok(Some(desc)) = descriptors::parse(&obu) {
            match desc {
                Descriptor::MixPresentation(mp) => info.ids.push(mp.mix_presentation_id),
                Descriptor::CodecConfig(cc) => info.codec = format!("{:?}", cc.codec_id),
                _ => {}
            }
        }
        if reader.is_empty() {
            break;
        }
    }
    info
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = std::io::stdout().execute(LeaveAlternateScreen);
}

#[allow(clippy::too_many_lines)]
fn run(name: &str, descriptors_bytes: Vec<u8>, media: Vec<u8>) -> Result<(), String> {
    let mix_info = parse_mix_info(&descriptors_bytes);
    let mut mix_index: Option<usize> = None; // None = automatic selection
    let mut renders = predecode_all(&descriptors_bytes, &media, MixSelection::Auto)?;

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no audio output device")?;
    let control = Arc::new(Control {
        mode: AtomicU8::new(1), // start on binaural: it's the demo
        paused: AtomicBool::new(false),
        pos: AtomicU64::new(0),
        seek: AtomicI64::new(0),
    });
    let mut stream = build_stream(&device, &renders, control.clone())?;
    stream.play().map_err(|e| format!("play: {e}"))?;

    enable_raw_mode().map_err(|e| e.to_string())?;
    std::io::stdout()
        .execute(EnterAlternateScreen)
        .map_err(|e| e.to_string())?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |p| {
        restore_terminal();
        default_hook(p);
    }));
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
            .map_err(|e| e.to_string())?;

    let result = loop {
        let rate = renders.stereo.rate;
        let total_frames = renders.stereo.pcm.len() as u64 / 2;
        let pos = control.pos.load(Ordering::Relaxed);
        let binaural_on = control.mode.load(Ordering::Relaxed) == 1;
        let paused = control.paused.load(Ordering::Relaxed);
        let peaks = bed_peaks(&renders.bed, pos, rate as usize / 20);

        let draw = terminal.draw(|f| {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Min(8),
                    Constraint::Length(2),
                ])
                .split(f.area());

            let mode_span = if binaural_on {
                Span::styled(
                    " BINAURAL (HRTF) ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    " STEREO ",
                    Style::default().fg(Color::Black).bg(Color::Gray),
                )
            };
            let header = Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(format!(
                        "  {}  mix {}  {} Hz  decoded {:.0}x realtime",
                        mix_info.codec,
                        renders.stereo.mix_id,
                        rate,
                        renders.binaural.realtime_factor,
                    )),
                ]),
                Line::from(vec![
                    mode_span,
                    Span::raw(format!(
                        "  {}{:>6.1}s / {:.1}s",
                        if paused { "⏸ " } else { "▶ " },
                        pos as f64 / f64::from(rate),
                        total_frames as f64 / f64::from(rate),
                    )),
                ]),
            ])
            .block(Block::default().borders(Borders::ALL).title(" iamfplay "));
            f.render_widget(header, rows[0]);

            let meter_block = Block::default()
                .borders(Borders::ALL)
                .title(" channel bed (rendered to 7.1.4) ");
            let inner = meter_block.inner(rows[1]);
            f.render_widget(meter_block, rows[1]);
            let columns = Layout::default()
                .direction(Direction::Vertical)
                .constraints(vec![Constraint::Length(1); peaks.len()])
                .split(inner);
            for (i, (&peak, area)) in peaks.iter().zip(columns.iter()).enumerate() {
                let label = BED_LABELS.get(i).copied().unwrap_or("?");
                let gauge =
                    Gauge::default()
                        .label(label)
                        .ratio(peak)
                        .gauge_style(Style::default().fg(if peak > 0.85 {
                            Color::Red
                        } else {
                            Color::Green
                        }));
                let row = Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: 1,
                };
                f.render_widget(gauge, row);
            }

            let help = Paragraph::new(
                "b: binaural/stereo   space: pause   ←/→: seek 2s   m: next mix   q: quit",
            )
            .style(Style::default().fg(Color::DarkGray));
            f.render_widget(help, rows[2]);
        });
        if let Err(e) = draw {
            break Err(e.to_string());
        }

        match event::poll(std::time::Duration::from_millis(33)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    // Raw mode swallows the SIGINT-generating keystroke, so
                    // honor Ctrl+C explicitly.
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break Ok(());
                    }
                    KeyCode::Char('b') => {
                        let mode = control.mode.load(Ordering::Relaxed) ^ 1;
                        control.mode.store(mode, Ordering::Relaxed);
                    }
                    KeyCode::Char(' ') => {
                        let paused = control.paused.load(Ordering::Relaxed);
                        control.paused.store(!paused, Ordering::Relaxed);
                    }
                    KeyCode::Left => {
                        control
                            .seek
                            .fetch_sub(i64::from(rate) * 2, Ordering::Relaxed);
                    }
                    KeyCode::Right => {
                        control
                            .seek
                            .fetch_add(i64::from(rate) * 2, Ordering::Relaxed);
                    }
                    KeyCode::Char('m') if mix_info.ids.len() > 1 => {
                        let next = mix_index.map_or(1, |i| (i + 1) % mix_info.ids.len());
                        drop(stream);
                        match predecode_all(&descriptors_bytes, &media, MixSelection::ByIndex(next))
                        {
                            Ok(next_renders) => {
                                renders = next_renders;
                                mix_index = Some(next);
                            }
                            Err(e) => break Err(e),
                        }
                        control.pos.store(0, Ordering::Relaxed);
                        stream = match build_stream(&device, &renders, control.clone()) {
                            Ok(s) => s,
                            Err(e) => break Err(e),
                        };
                        if let Err(e) = stream.play() {
                            break Err(format!("play: {e}"));
                        }
                    }
                    _ => {}
                },
                Ok(_) => {}
                Err(e) => break Err(e.to_string()),
            },
            Ok(false) => {}
            Err(e) => break Err(e.to_string()),
        }
    };
    restore_terminal();
    result
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: iamfplay <file.iamf|file.mp4>   play a stream (headphones recommended)\n\
         \x20      iamfplay --demo [-o out.iamf]   play a generated 3OA binaural demo scene\n\
         \x20      iamfplay --check <file>         decode all renders, print a summary, exit"
    );
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut use_demo = false;
    let mut check_only = false;
    let mut write_out = None;
    let mut path = None;
    while !args.is_empty() {
        match args.remove(0).as_str() {
            "--demo" => use_demo = true,
            "--check" => check_only = true,
            "-o" if !args.is_empty() => write_out = Some(args.remove(0)),
            arg if path.is_none() && !arg.starts_with('-') => path = Some(arg.to_string()),
            _ => return usage(),
        }
    }

    let (name, data) = if use_demo {
        ("demo: bee orbit (3OA)".to_string(), demo::generate())
    } else {
        let Some(path) = path else {
            return usage();
        };
        match std::fs::read(&path) {
            Ok(data) => (path, data),
            Err(e) => {
                eprintln!("error: cannot read input: {e}");
                return ExitCode::FAILURE;
            }
        }
    };
    if let Some(out) = write_out {
        if let Err(e) = std::fs::write(&out, &data) {
            eprintln!("error: cannot write {out}: {e}");
            return ExitCode::FAILURE;
        }
        println!("wrote {} bytes to {out}", data.len());
    }

    let (descriptors_bytes, media) = if mp4::is_mp4(&data) {
        match mp4::demux(&data) {
            Ok(demuxed) => (demuxed.descriptors, demuxed.media),
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        match descriptor_split(&data) {
            Some(split) => (data[..split].to_vec(), data[split..].to_vec()),
            None => {
                eprintln!("error: not a parseable IAMF stream or MP4 file");
                return ExitCode::FAILURE;
            }
        }
    };

    if check_only {
        let mix_info = parse_mix_info(&descriptors_bytes);
        return match predecode_all(&descriptors_bytes, &media, MixSelection::Auto) {
            Ok(renders) => {
                println!(
                    "{name}: {}  mix {} of {:?}  {} Hz  {:.1}s  stereo/binaural/bed ok  \
                     binaural decoded {:.0}x realtime",
                    mix_info.codec,
                    renders.stereo.mix_id,
                    mix_info.ids,
                    renders.stereo.rate,
                    renders.stereo.pcm.len() as f64 / 2.0 / f64::from(renders.stereo.rate),
                    renders.binaural.realtime_factor,
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    match run(&name, descriptors_bytes, media) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
