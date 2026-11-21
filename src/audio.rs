//! Desktop audio: Opus, 48 kHz stereo in 20 ms packets, on its own broadcast.
//!
//! A source publishes `config` (one JSON frame) and `audio`: each frame is a u64 BE microsecond
//! timestamp followed by one Opus packet; a group holds about a second of packets, so a
//! listener joins within one. `receive-audio` plays a source on the default output device
//! through a small jitter buffer:
//!
//!   petplay-media receive-audio SHARE_CODE
//!   petplay-media receive-audio https://HOST:PORT/ CERT_SHA256
//!
//! It prints `AUDIO_PLAYING device=… rate=…` once the output runs and `AUDIO_STATS` every
//! five seconds.
use crate::publish::{Transport, serve};
use anyhow::{Context, Result, ensure};
use moq_net::{Timescale, Timestamp, group, origin, track};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one 20 ms packet.
pub const FRAME: usize = 960;
const PACKETS_PER_GROUP: u32 = 50;

// Linux publishes once its capture lands; Windows already does.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct AudioPublisher {
    pub fingerprint: String,
    _runtime: tokio::runtime::Runtime,
    _origin: origin::Producer,
    _broadcast: moq_net::broadcast::Producer,
    _config: track::Producer,
    audio: track::Producer,
    group: Option<group::Producer>,
    sequence: u64,
    in_group: u32,
    encoder: opus::Encoder,
    packet: Vec<u8>,
}

impl AudioPublisher {
    pub fn new(bind: SocketAddr, transport: Transport, bitrate: i32) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let _enter = runtime.enter();
        let _ = moq_tokio::crypto::install_default();
        let origin = moq_tokio::origin::spawn();
        let broadcast = origin.create_broadcast("petplay")?;
        let config = broadcast.create_track("config", None)?;
        let audio = broadcast.create_track(
            "audio",
            track::Info::default()
                .with_timescale(Timescale::MICRO)
                .with_max_age(Duration::from_secs(1)),
        )?;
        broadcast.announce(Default::default())?;
        let mut group = config.append_group()?;
        group.write_frame(
            Timestamp::now(),
            format!(
                "{{\"version\":1,\"kind\":\"audio\",\"codec\":\"opus\",\"sampleRate\":{RATE},\"channels\":{CHANNELS},\"frameSamples\":{FRAME},\"timestamp\":\"microseconds\"}}"
            ),
        )?;
        group.finish()?;
        let (fingerprint, _code) = serve(&runtime, origin.consume(), bind, transport)?;
        drop(_enter);
        let mut encoder =
            opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio)?;
        encoder.set_bitrate(opus::Bitrate::Bits(bitrate))?;
        Ok(Self {
            fingerprint,
            _runtime: runtime,
            _origin: origin,
            _broadcast: broadcast,
            _config: config,
            audio,
            group: None,
            sequence: 0,
            in_group: 0,
            encoder,
            packet: vec![0; 4000],
        })
    }

    /// Encode and publish one packet: `FRAME` interleaved stereo samples per channel.
    pub fn publish(&mut self, samples: &[f32], micros: u64) -> Result<usize> {
        ensure!(
            samples.len() == FRAME * CHANNELS,
            "one packet is {FRAME} stereo samples"
        );
        let len = self.encoder.encode_float(samples, &mut self.packet)?;
        if self.group.is_none() || self.in_group >= PACKETS_PER_GROUP {
            if let Some(g) = self.group.take() {
                g.finish()?;
            }
            self.group = Some(self.audio.create_group(self.sequence.into())?);
            self.sequence += 1;
            self.in_group = 0;
        }
        let mut data = Vec::with_capacity(len + 8);
        data.extend_from_slice(&micros.to_be_bytes());
        data.extend_from_slice(&self.packet[..len]);
        self.group
            .as_mut()
            .unwrap()
            .write_frame(Timestamp::from_scale(micros, 1_000_000)?, data)?;
        self.in_group += 1;
        Ok(len)
    }
}

/// Decoded audio waiting for the output, interleaved stereo at `RATE`.
struct Jitter {
    buffer: VecDeque<f32>,
    /// Waiting to fill up to `TARGET` before playing again.
    priming: bool,
    /// Fractional read position (in frames) for resampling to the device rate.
    position: f64,
    underruns: u64,
    dropped: u64,
}

/// Played this far behind the newest packet: absorbs network jitter.
const TARGET: usize = RATE as usize * 80 / 1000;
/// More than this behind and the excess is dropped, so latency cannot creep up.
const CEILING: usize = RATE as usize * 200 / 1000;

impl Jitter {
    fn push(&mut self, samples: &[f32]) {
        self.buffer.extend(samples);
        let frames = self.buffer.len() / CHANNELS;
        if frames > CEILING {
            let drop = (frames - TARGET) * CHANNELS;
            self.buffer.drain(..drop);
            self.dropped += (drop / CHANNELS) as u64;
        }
    }

    /// Fill `out` (`channels` interleaved, at `rate`) from the buffer.
    fn fill<T: cpal::SizedSample + cpal::FromSample<f32>>(
        &mut self,
        out: &mut [T],
        channels: usize,
        rate: u32,
    ) {
        // Two clocks never agree exactly: play up to 0.5% faster or slower (inaudible) to hold
        // the buffer at its target instead of letting it drain or creep up.
        let behind = (self.buffer.len() / CHANNELS) as f64 - TARGET as f64;
        let correction = (behind / TARGET as f64 * 0.005).clamp(-0.005, 0.005);
        let step = RATE as f64 / rate as f64 * (1.0 + correction);
        if self.priming && self.buffer.len() / CHANNELS >= TARGET {
            self.priming = false;
        }
        for frame in out.chunks_mut(channels) {
            let available = self.buffer.len() / CHANNELS;
            let index = self.position as usize;
            let (left, right) = if !self.priming && index + 1 < available {
                let t = (self.position - index as f64) as f32;
                let at = |i: usize, c: usize| self.buffer[i * CHANNELS + c];
                (
                    at(index, 0) + (at(index + 1, 0) - at(index, 0)) * t,
                    at(index, 1) + (at(index + 1, 1) - at(index, 1)) * t,
                )
            } else {
                if !self.priming {
                    self.priming = true;
                    self.underruns += 1;
                }
                (0.0, 0.0)
            };
            if !self.priming {
                self.position += step;
            }
            for (c, sample) in frame.iter_mut().enumerate() {
                let value = match c {
                    0 => left,
                    1 => right,
                    _ => 0.0,
                };
                *sample = T::from_sample(value);
            }
        }
        let consumed = (self.position as usize).min(self.buffer.len() / CHANNELS);
        self.buffer.drain(..consumed * CHANNELS);
        self.position -= consumed as f64;
    }
}

fn open_output(jitter: Arc<Mutex<Jitter>>) -> Result<cpal::Stream> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no audio output device")?;
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let (channels, rate) = (config.channels as usize, config.sample_rate.0);
    let error = |e| eprintln!("audio output: {e}");
    fn build<T: cpal::SizedSample + cpal::FromSample<f32>>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        jitter: Arc<Mutex<Jitter>>,
        error: impl FnMut(cpal::StreamError) + Send + 'static,
    ) -> Result<cpal::Stream> {
        let (channels, rate) = (config.channels as usize, config.sample_rate.0);
        Ok(device.build_output_stream(
            config,
            move |out: &mut [T], _| jitter.lock().unwrap().fill(out, channels, rate),
            error,
            None,
        )?)
    }
    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, jitter, error)?,
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, jitter, error)?,
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, jitter, error)?,
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, jitter, error)?,
        other => anyhow::bail!("unsupported output sample format {other}"),
    };
    stream.play()?;
    println!(
        "AUDIO_PLAYING device={:?} rate={rate} channels={channels}",
        device.name().unwrap_or_default()
    );
    Ok(stream)
}

pub fn receive(args: &[std::ffi::OsString]) -> Result<()> {
    ensure!(
        (1..=2).contains(&args.len()),
        "usage: petplay-media receive-audio SHARE_CODE | https://HOST:PORT/ CERT_SHA256"
    );
    // `subscribe` takes the video receiver's arguments, which end in an output socket.
    let mut connect = args.to_vec();
    connect.push("-".into());
    let jitter = Arc::new(Mutex::new(Jitter {
        buffer: VecDeque::new(),
        priming: true,
        position: 0.0,
        underruns: 0,
        dropped: 0,
    }));
    let _output = open_output(jitter.clone())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let sink = jitter.clone();
    let network = crate::receive_input::subscribe(
        &runtime,
        &connect,
        move |broadcast| async move {
            let mut catalog = broadcast.track("config")?.subscribe(None).await?;
            let mut group = catalog
                .recv_group()
                .await?
                .context("missing config group")?;
            let frame = group.read_frame().await?.context("missing config frame")?;
            ensure!(frame.payload.len() <= 65536, "config too large");
            let config: serde_json::Value = serde_json::from_slice(&frame.payload)?;
            ensure!(
                config["version"] == 1
                    && config["kind"] == "audio"
                    && config["codec"] == "opus"
                    && config["sampleRate"] == RATE
                    && config["channels"] == CHANNELS,
                "not a 48 kHz stereo Opus audio source"
            );
            println!("AUDIO_CONNECTED");
            let mut decoder = opus::Decoder::new(RATE, opus::Channels::Stereo)?;
            // Up to 120 ms per packet.
            let mut pcm = vec![0f32; 5760 * CHANNELS];
            let mut audio = broadcast.track("audio")?.subscribe(None).await?;
            let (mut packets, mut reported) = (0u64, Instant::now());
            // The longest wait between two packets since the last report: network jitter.
            let (mut arrived, mut max_gap) = (Instant::now(), Duration::ZERO);
            while let Some(mut group) = audio.recv_group().await? {
                while let Ok(Some(frame)) = group.read_frame().await {
                    let payload = frame.payload;
                    ensure!((9..=8000).contains(&payload.len()), "invalid audio packet");
                    let frames = decoder.decode_float(&payload[8..], &mut pcm, false)?;
                    sink.lock().unwrap().push(&pcm[..frames * CHANNELS]);
                    packets += 1;
                    max_gap = max_gap.max(arrived.elapsed());
                    arrived = Instant::now();
                    if reported.elapsed() >= Duration::from_secs(5) {
                        let j = sink.lock().unwrap();
                        println!(
                            "AUDIO_STATS packets={packets} buffered_ms={} underruns={} dropped_ms={} max_gap_ms={}",
                            j.buffer.len() / CHANNELS * 1000 / RATE as usize,
                            j.underruns,
                            j.dropped * 1000 / RATE as u64,
                            max_gap.as_millis()
                        );
                        reported = Instant::now();
                        max_gap = Duration::ZERO;
                    }
                }
            }
            Ok(())
        },
    )?;
    runtime.block_on(network)??;
    Ok(())
}
