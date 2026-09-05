//! C03 integration helper.
//!
//! The `filter` role owns the native `pw_filter` node. `tone` and `capture`
//! are independent PipeWire stream clients used by the shell proof to make
//! explicit links and measure a mixed stereo signal.

use anyhow::{bail, Context, Result};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const FRAME_BYTES: usize = std::mem::size_of::<f32>() * CHANNELS as usize;

fn format_param_bytes() -> Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32P);
    info.set_rate(RATE);
    info.set_channels(CHANNELS);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);

    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: pw::spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .context("serializing C03 stream format")?
    .0
    .into_inner();
    Ok(values)
}

fn run_for(mainloop: &pw::main_loop::MainLoopRc, duration: Duration) -> Result<()> {
    let done_loop = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| done_loop.quit());
    timer
        .update_timer(Some(duration), None)
        .into_result()
        .context("arming bounded C03 helper timer")?;
    mainloop.run();
    Ok(())
}

fn connect_context() -> Result<(
    pw::main_loop::MainLoopRc,
    pw::context::ContextRc,
    pw::core::CoreRc,
)> {
    pw::init();
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).context("creating helper PipeWire main loop")?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).context("creating helper PipeWire context")?;
    let core = context
        .connect_rc(None)
        .context("connecting helper to PipeWire")?;
    Ok((mainloop, context, core))
}

fn run_tone(node_name: &str, left_hz: f32, right_hz: f32, duration: Duration) -> Result<()> {
    let (mainloop, _context, core) = connect_context()?;
    let stream = pw::stream::StreamBox::new(
        &core,
        node_name,
        properties! {
            "node.name" => node_name,
            "node.description" => node_name,
            "media.type" => "Audio",
            "media.category" => "Playback",
            "media.role" => "Music",
            "target.object" => "peq",
            "stream.is-live" => "true",
        },
    )
    .context("creating tone stream")?;
    let format_bytes = format_param_bytes()?;
    let format = Pod::from_bytes(&format_bytes).context("building tone format POD")?;
    let mut params = [format];
    let tone_frames = Arc::new(AtomicU64::new(0));
    let mut state = ToneState {
        phases: [0.0, 0.0],
        frames: tone_frames.clone(),
    };
    let _listener = stream
        .add_local_listener_with_user_data(&mut state)
        .state_changed(|_, _, old, new| {
            eprintln!("tone stream state {old:?} -> {new:?}");
        })
        .process(move |stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let capacity = data.data().map_or(0, |bytes| bytes.len() / FRAME_BYTES);
            if capacity == 0 {
                return;
            }
            if let Some(bytes) = data.data() {
                let frames = capacity.min(bytes.len() / FRAME_BYTES);
                for frame in 0..frames {
                    let left = state.phases[0].sin() * 0.20;
                    let right = state.phases[1].sin() * 0.20;
                    let offset = frame * FRAME_BYTES;
                    bytes[offset..offset + 4].copy_from_slice(&left.to_ne_bytes());
                    bytes[offset + 4..offset + 8].copy_from_slice(&right.to_ne_bytes());
                    state.phases[0] += std::f32::consts::TAU * left_hz / RATE as f32;
                    state.phases[1] += std::f32::consts::TAU * right_hz / RATE as f32;
                    if state.phases[0] >= std::f32::consts::TAU {
                        state.phases[0] -= std::f32::consts::TAU;
                    }
                    if state.phases[1] >= std::f32::consts::TAU {
                        state.phases[1] -= std::f32::consts::TAU;
                    }
                }
                state.frames.fetch_add(frames as u64, Ordering::Relaxed);
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = FRAME_BYTES as i32;
                *chunk.size_mut() = (frames * FRAME_BYTES) as u32;
            }
        })
        .register()
        .context("registering tone process callback")?;
    stream
        .connect(
            spa::utils::Direction::Output,
            std::env::var("PEQ_TARGET_ID")
                .ok()
                .and_then(|value| value.parse::<u32>().ok()),
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .context("connecting tone stream")?;
    run_for(&mainloop, duration)?;
    println!(
        "tone node={node_name} frames={}",
        tone_frames.load(Ordering::Relaxed)
    );
    Ok(())
}

struct ToneState {
    phases: [f32; 2],
    frames: Arc<AtomicU64>,
}

#[derive(Default)]
struct CaptureStats {
    samples: [AtomicU64; 2],
    sum_sq_scaled: [AtomicU64; 2],
    process_calls: AtomicU64,
    stream_error: AtomicBool,
}

fn run_capture(node_name: &str, duration: Duration) -> Result<()> {
    let (mainloop, _context, core) = connect_context()?;
    let stream = pw::stream::StreamBox::new(
        &core,
        node_name,
        properties! {
            "node.name" => node_name,
            "node.description" => node_name,
            "media.type" => "Audio",
            "media.category" => "Capture",
            "media.role" => "Analysis",
            "target.object" => "peq",
            "stream.is-live" => "true",
        },
    )
    .context("creating capture stream")?;
    let format_bytes = format_param_bytes()?;
    let format = Pod::from_bytes(&format_bytes).context("building capture format POD")?;
    let mut params = [format];
    let stats = Arc::new(CaptureStats::default());
    let callback_stats = stats.clone();
    let _listener = stream
        .add_local_listener_with_user_data(callback_stats)
        .state_changed(|_, stats, _, state| {
            if matches!(state, pw::stream::StreamState::Error(_)) {
                stats.stream_error.store(true, Ordering::Relaxed);
            }
        })
        .process(|stream, stats| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let chunk_size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else {
                return;
            };
            let bytes = &bytes[..chunk_size.min(bytes.len())];
            for frame in bytes.chunks_exact(FRAME_BYTES) {
                for (channel, raw) in [
                    [frame[0], frame[1], frame[2], frame[3]],
                    [frame[4], frame[5], frame[6], frame[7]],
                ]
                .into_iter()
                .enumerate()
                {
                    let sample = f32::from_ne_bytes(raw);
                    let square = (sample as f64 * sample as f64 * 1_000_000_000.0) as u64;
                    stats.sum_sq_scaled[channel].fetch_add(square, Ordering::Relaxed);
                    stats.samples[channel].fetch_add(1, Ordering::Relaxed);
                }
            }
            stats.process_calls.fetch_add(1, Ordering::Relaxed);
        })
        .register()
        .context("registering capture process callback")?;
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .context("connecting capture stream")?;
    run_for(&mainloop, duration)?;
    if stats.stream_error.load(Ordering::Relaxed) {
        bail!("capture stream entered an error state")
    }
    println!(
        "capture node={node_name} process_calls={} FL samples={} rms={:.6} FR samples={} rms={:.6}",
        stats.process_calls.load(Ordering::Relaxed),
        stats.samples[0].load(Ordering::Relaxed),
        rms(&stats, 0),
        stats.samples[1].load(Ordering::Relaxed),
        rms(&stats, 1),
    );
    Ok(())
}

fn rms(stats: &CaptureStats, channel: usize) -> f64 {
    let samples = stats.samples[channel].load(Ordering::Relaxed);
    if samples == 0 {
        return 0.0;
    }
    let sum = stats.sum_sq_scaled[channel].load(Ordering::Relaxed) as f64 / 1_000_000_000.0;
    (sum / samples as f64).sqrt()
}

fn parse_duration(value: Option<&String>) -> Result<Duration> {
    let millis = value
        .map(|s| s.parse::<u64>())
        .transpose()
        .context("duration must be an integer number of milliseconds")?
        .unwrap_or(4_000);
    Ok(Duration::from_millis(millis))
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("filter") => {
            let duration = parse_duration(args.next().as_ref())?;
            let filter = peq::native_audio::NativePwFilter::new()?;
            println!("filter node_id={} node=peq", filter.node_id());
            filter.run_for(duration);
            println!(
                "filter done sample_rate={:?} process_calls={}",
                filter.effective_sample_rate(),
                filter.process_calls()
            );
        }
        Some("source") => {
            let node = args.next().context("source needs a node name")?;
            let left = args
                .next()
                .context("source needs a left frequency")?
                .parse::<f32>()
                .context("source left frequency must be a number")?;
            let right = args
                .next()
                .context("source needs a right frequency")?
                .parse::<f32>()
                .context("source right frequency must be a number")?;
            let source = peq::native_audio::NativePwSource::new(&node, left, right)?;
            println!("source node_id={} node={node}", source.node_id());
            source.run_for(parse_duration(args.next().as_ref())?);
            println!(
                "source done node={node} sample_rate={:?} process_calls={} frames={}",
                source.effective_sample_rate(),
                source.process_calls(),
                source.frames()
            );
        }
        Some("tone") => {
            let node = args.next().context("tone needs a node name")?;
            let left = args
                .next()
                .context("tone needs a left frequency")?
                .parse::<f32>()
                .context("left frequency must be a number")?;
            let right = args
                .next()
                .context("tone needs a right frequency")?
                .parse::<f32>()
                .context("right frequency must be a number")?;
            run_tone(&node, left, right, parse_duration(args.next().as_ref())?)?;
        }
        Some("capture") => {
            let node = args.next().context("capture needs a node name")?;
            run_capture(&node, parse_duration(args.next().as_ref())?)?;
        }
        _ => bail!(
            "usage: native_pw_proof filter [ms] | source NODE LEFT_HZ RIGHT_HZ [ms] | tone NODE LEFT_HZ RIGHT_HZ [ms] | capture NODE [ms]"
        ),
    }
    Ok(())
}
