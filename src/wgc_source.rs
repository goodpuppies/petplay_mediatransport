//! `capture-wgc`: one Windows monitor, captured, converted and H.264-encoded on the GPU, and
//! published like the Linux encoder's stream. No pixel is read back to the CPU.
//!
//!   petplay-media capture-wgc --list
//!   petplay-media capture-wgc MONITOR [LISTEN_ADDR] [--iroh-share] [--fps N]
//!       [--bitrate BITS_PER_SECOND] [--keyframe-seconds N] [--no-cursor]
//!
//! `MONITOR` is an index from `--list`. `LISTEN_ADDR` (default 127.0.0.1:0) serves browsers and
//! pinned native receivers; `--iroh-share` also prints a share code reachable across the internet.
//! Readiness and progress lines match the Linux encoder's (`MOQ_READY`, `MEDIA_SOURCE`).
//!
//! Windows sends frames only when the monitor changes. A still screen repeats its last picture a
//! few times (a decoder may hold its newest picture until more arrive), then once a second, with
//! a keyframe once per keyframe interval so a viewer joining a still screen gets a picture.
use crate::publish::{Format, Layout, Publisher, Transport};
use crate::windows_encode::{Converter, Encoder, Event, Platform, Settings, device};
use anyhow::{Context, Result, bail, ensure};
use petplay_capture::windows::{
    Capture, focus_window, monitors, use_physical_pixels, windows as list_windows,
};
use serde_json::json;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: petplay-media capture-wgc --list | --list-windows | --focus-window HWND\n   or: petplay-media capture-wgc MONITOR|--window HWND [LISTEN_ADDR] [--iroh-share] [--fps N] [--bitrate BITS_PER_SECOND] [--keyframe-seconds N] [--no-cursor]";
/// Repeats of the last picture once the screen goes still, a frame interval apart.
const IDLE_REPEATS: u32 = 4;
/// Then one repeat a second.
const HEARTBEAT: Duration = Duration::from_secs(1);

pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    use_physical_pixels();
    let mut positional = Vec::new();
    let (mut fps, mut bitrate, mut keyframe_seconds) = (30u32, 8_000_000u32, 1u32);
    let (mut share, mut cursor, mut list, mut list_win) = (false, true, false, false);
    let mut window_hwnd: Option<isize> = None;
    let mut focus_hwnd: Option<isize> = None;
    let mut rest = args.iter();
    let parse_u32 = |rest: &mut std::slice::Iter<'_, std::ffi::OsString>, name: &str| -> Result<u32> {
        rest.next()
            .with_context(|| format!("{name} needs a value"))?
            .to_str()
            .context("non-UTF-8 option value")?
            .parse()
            .with_context(|| name.to_owned())
    };
    let parse_hwnd = |rest: &mut std::slice::Iter<'_, std::ffi::OsString>, name: &str| -> Result<isize> {
        let raw = rest
            .next()
            .with_context(|| format!("{name} needs a value"))?
            .to_str()
            .context("non-UTF-8 option value")?;
        if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
            isize::from_str_radix(hex, 16).with_context(|| name.to_owned())
        } else {
            raw.parse().with_context(|| name.to_owned())
        }
    };
    while let Some(arg) = rest.next() {
        let text = arg.to_str().context("non-UTF-8 argument")?;
        match text {
            "--list" => list = true,
            "--list-windows" => list_win = true,
            "--window" => window_hwnd = Some(parse_hwnd(&mut rest, "--window")?),
            "--focus-window" => focus_hwnd = Some(parse_hwnd(&mut rest, "--focus-window")?),
            "--iroh-share" => share = true,
            "--no-cursor" => cursor = false,
            "--fps" => fps = parse_u32(&mut rest, "--fps")?,
            "--bitrate" => bitrate = parse_u32(&mut rest, "--bitrate")?,
            "--keyframe-seconds" => keyframe_seconds = parse_u32(&mut rest, "--keyframe-seconds")?,
            _ if text.starts_with("--") => bail!("unknown option {text}\n{USAGE}"),
            _ => positional.push(text.to_owned()),
        }
    }
    if let Some(hwnd) = focus_hwnd {
        focus_window(hwnd);
        return Ok(());
    }
    if list_win {
        let items: Vec<_> = list_windows()
            .into_iter()
            .map(|w| {
                json!({
                    "id": format!("0x{:x}", w.handle as usize),
                    "title": w.title,
                    "app": w.app,
                    "pid": w.pid,
                    "x": w.x,
                    "y": w.y,
                    "width": w.width,
                    "height": w.height,
                    "minimized": w.minimized,
                    "active": w.active,
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(items));
        return Ok(());
    }
    if list {
        for (index, monitor) in monitors().iter().enumerate() {
            println!(
                "WGC_MONITOR index={index} name={:?} x={} y={} width={} height={} primary={}",
                monitor.name, monitor.x, monitor.y, monitor.width, monitor.height, monitor.primary as u8
            );
        }
        return Ok(());
    }
    ensure!(
        (1..=240).contains(&fps)
            && (100_000..=200_000_000).contains(&bitrate)
            && (1..=60).contains(&keyframe_seconds),
        "fps, bitrate or keyframe interval out of range"
    );
    let (label, bind, target_window) = if let Some(hwnd) = window_hwnd {
        ensure!(positional.len() <= 1, "{USAGE}");
        let bind = positional.first().map(String::as_str).unwrap_or("127.0.0.1:0").parse()?;
        (format!("window:0x{:x}", hwnd as usize), bind, Some(hwnd))
    } else {
        ensure!((1..=2).contains(&positional.len()), "{USAGE}");
        let index: usize = positional[0].parse().context("MONITOR is an index from --list")?;
        let monitor = monitors()
            .into_iter()
            .nth(index)
            .with_context(|| format!("no monitor {index}; see capture-wgc --list"))?;
        let bind = positional.get(1).map(String::as_str).unwrap_or("127.0.0.1:0").parse()?;
        (monitor.name, bind, None)
    };

    let _platform = Platform::new()?;
    let (device, context) = device()?;
    let mut capture = if let Some(hwnd) = target_window {
        Capture::start_window(&device, hwnd, cursor).context("start Windows Graphics Capture (window)")?
    } else {
        let index: usize = positional[0].parse()?;
        let monitor = monitors()
            .into_iter()
            .nth(index)
            .with_context(|| format!("no monitor {index}"))?;
        Capture::start(&device, monitor.handle, cursor).context("start Windows Graphics Capture")?
    };
    // Windows sends the target's current picture as soon as capture starts.
    let deadline = Instant::now() + Duration::from_secs(5);
    let first = loop {
        if let Some(frame) = capture.next_frame(Duration::from_millis(100))? {
            break frame;
        }
        ensure!(Instant::now() < deadline, "no picture from {} within 5 s", label);
    };
    // NV12 needs even dimensions: an odd capture loses its last row or column.
    let (width, height) = (first.width & !1, first.height & !1);
    ensure!(width >= 16 && height >= 16, "target too small: {}x{}", first.width, first.height);
    let mut converter = Converter::new(&device, &context, width, height)?;
    let mut encoder = Encoder::new(
        &device,
        &Settings { width, height, fps, bitrate, gop: fps * keyframe_seconds },
    )?;
    let mut publisher = Publisher::new(bind, if share { Transport::Peer } else { Transport::Browser })?;
    println!("MOQ_READY listen={bind} fingerprint={}", publisher.fingerprint);
    publisher.describe(Format { width, height, framerate: fps, layout: Layout::Mono });
    println!("MEDIA_SOURCE width={width} height={height} fps={fps} layout=Mono monitor={:?}", label);

    let start = Instant::now();
    let ticks = |at: Instant| (at - start).as_nanos() as i64 / 100;
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let keyframe_period = Duration::from_secs(keyframe_seconds as u64);
    let mut pending = Some(first);
    let mut last = None;
    let (mut repeats, mut last_submit, mut last_key) = (0u32, start, start);
    // Inputs the encoder asked for, and submitted inputs it has not returned yet.
    let (mut wanted, mut in_flight) = (0u32, 0u32);
    let (mut captured, mut repeated, mut published, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    let mut reported = Instant::now();
    loop {
        while let Some(event) = encoder.poll_event()? {
            match event {
                Event::NeedInput => wanted += 1,
                Event::HaveOutput => {
                    in_flight = in_flight.saturating_sub(1);
                    let Some(unit) = encoder.take()? else { continue };
                    if unit.key {
                        last_key = Instant::now();
                    }
                    // A viewer starts at a keyframe; so does the stream.
                    if published == 0 && !unit.key {
                        continue;
                    }
                    publisher.frame(&unit.bytes, unit.key, unit.time.max(0) as u64 * 100)?;
                    published += 1;
                    bytes += unit.bytes.len() as u64;
                }
                Event::Other => {}
            }
        }
        if reported.elapsed() >= Duration::from_secs(5) {
            println!(
                "WGC_STATS captured={captured} repeated={repeated} published={published} kbps={}",
                bytes * 8 / 1000 / reported.elapsed().as_secs().max(1)
            );
            bytes = 0;
            reported = Instant::now();
        }
        if wanted == 0 {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        // Waiting on capture is the idle wait, unless output is due.
        let wait = if in_flight > 0 {
            Duration::from_millis(1)
        } else if repeats > 0 {
            interval
        } else {
            HEARTBEAT.saturating_sub(last_submit.elapsed()).max(Duration::from_millis(1))
        };
        let frame = match pending.take() {
            Some(frame) => Some(frame),
            None => capture.next_frame(wait)?,
        };
        let now = Instant::now();
        let surface = if let Some(frame) = frame {
            ensure!(
                frame.width & !1 == width && frame.height & !1 == height,
                "{} changed resolution to {}x{}; restart the capture",
                label,
                frame.width,
                frame.height
            );
            let surface = converter.convert(&frame.texture)?;
            // The texture goes back to Windows only after the GPU has read it.
            drop(frame);
            captured += 1;
            repeats = IDLE_REPEATS;
            last = Some(surface.clone());
            (surface, false)
        } else {
            let Some(surface) = last.clone() else { continue };
            if repeats > 0 {
                repeats -= 1;
            } else if now - last_submit < HEARTBEAT {
                continue;
            }
            repeated += 1;
            // A still screen still gets a keyframe per interval, for viewers joining it.
            (surface, now - last_key >= keyframe_period)
        };
        encoder.submit(&surface.0, ticks(now), surface.1)?;
        wanted -= 1;
        in_flight += 1;
        last_submit = now;
    }
}
