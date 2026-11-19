//! Linux DMA-BUF ingestion, GPU encoding, recording and publication.
//!
//! This is the half of the worker that needs a VA-API device, so it is behind the
//! `encode` feature. A receive-only build (the `gateway` subcommand, for a viewer on a
//! platform with no VA-API) leaves it out.
use anyhow::{Context, Result, bail, ensure};
use moq_vaapi::{
    dmabuf::{DmaBuf, Plane, drm},
    encode::{Config, Encoder},
};
use std::{
    fs::{self, File},
    io::Write,
    mem::{self, size_of},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use crate::{publish, web};
const FRAME: usize = 64;
const MAGIC: u32 = 0x50504d31;
fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn u64at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
fn syscall(n: libc::c_int, what: &str) -> Result<libc::c_int> {
    if n < 0 {
        Err(std::io::Error::last_os_error()).context(what.to_owned())
    } else {
        Ok(n)
    }
}
fn owned(n: libc::c_int) -> OwnedFd {
    unsafe { OwnedFd::from_raw_fd(n) }
}
struct SocketPath {
    path: PathBuf,
    inode: u64,
}
impl Drop for SocketPath {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
/// Returns a slot to the producer: its reuse permission for this exact slot and sequence.
fn release(client: &OwnedFd, slot: usize, seq: u64) -> std::io::Result<()> {
    let mut ack = [0u8; 16];
    ack[..4].copy_from_slice(&MAGIC.to_le_bytes());
    ack[4..8].copy_from_slice(&(slot as u32).to_le_bytes());
    ack[8..].copy_from_slice(&seq.to_le_bytes());
    let n = unsafe {
        libc::send(
            client.as_raw_fd(),
            ack.as_ptr().cast(),
            ack.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if n != ack.len() as isize {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn listen(path: &Path) -> Result<(OwnedFd, SocketPath)> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    ensure!(
        !path.exists() && !bytes.contains(&0) && bytes.len() < 108,
        "socket exists or path invalid"
    );
    let fd = owned(syscall(
        unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) },
        "socket",
    )?);
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i] = *b as _;
    }
    syscall(
        unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &addr as *const _ as *const _,
                size_of::<libc::sockaddr_un>() as _,
            )
        },
        "bind",
    )?;
    let guard = SocketPath {
        path: path.into(),
        inode: fs::symlink_metadata(path)?.ino(),
    };
    syscall(unsafe { libc::listen(fd.as_raw_fd(), 1) }, "listen")?;
    Ok((fd, guard))
}
fn receive(fd: &OwnedFd) -> Result<Option<([u8; FRAME], Vec<OwnedFd>)>> {
    let mut bytes = [0u8; FRAME];
    let mut control = [0usize; 8];
    let mut io = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut io;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = size_of_val(&control);
    let n = unsafe { libc::recvmsg(fd.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(std::io::Error::last_os_error()).context("receive frame");
    }
    let mut fds = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                ensure!(
                    (*c).cmsg_len >= libc::CMSG_LEN(0) as usize,
                    "short rights header"
                );
                let count = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize) / size_of::<i32>();
                let data = libc::CMSG_DATA(c).cast::<i32>();
                for i in 0..count {
                    fds.push(owned(*data.add(i)));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if n == 0 && fds.is_empty() {
        return Ok(None);
    }
    ensure!(
        n as usize == FRAME
            && msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0
            && u32at(&bytes, 12) <= 1
            && fds.len() == if u32at(&bytes, 12) == 1 { 1 } else { 2 },
        "invalid frame or FD count"
    );
    ensure!(
        u32at(&bytes, 0) == MAGIC && u32at(&bytes, 4) == 1,
        "unsupported protocol"
    );
    Ok(Some((bytes, fds)))
}
fn wait_ready(fd: &OwnedFd) -> Result<()> {
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    syscall(
        unsafe { libc::poll(&mut p, 1, 2000) },
        "wait producer GPU fence",
    )?;
    ensure!(
        p.revents & libc::POLLIN != 0 && p.revents & (libc::POLLERR | libc::POLLNVAL) == 0,
        "producer GPU fence timed out or failed"
    );
    Ok(())
}
/// Run the encoding source. `args` is the full argument list, including the program name.
#[path = "pose_ring.rs"]
mod pose_ring;
pub use pose_ring::Pose;
pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    let usage = "usage: petplay-media SOCKET OUTPUT.h264 [QUIC_LISTEN_ADDR] \
        [--iroh-local|--iroh-share] [--fps N] [--bitrate BITS_PER_SECOND] \
        [--layout mono|side-by-side] [--pose-ring PATH] [--pose-lag-us N] [--keyframe-seconds N] \
        [--persist] [--max-fps N]";
    let mut positional: Vec<&std::ffi::OsStr> = Vec::new();
    let mut transport = publish::Transport::Browser;
    let mut framerate = 75u32;
    let mut bitrate = 12_000_000u32;
    let mut layout = publish::Layout::SideBySide;
    // The driver's head-pose ring. It latches one pose per display frame shortly before each
    // tick, so a frame's pose is the newest latched at capture (minus an optional lag).
    let mut pose_path: Option<std::path::PathBuf> = None;
    let mut pose_lag_us = 0u64;
    // A keyframe costs an encode spike and a burst of bytes; late joiners wait up to this long.
    let mut keyframe_seconds = 1u32;
    // Keep serving after a producer leaves: accept the next one on the same socket, with the same
    // encoder and broadcast. For long-lived feeds whose capture restarts (a desktop capture
    // helper); without it the worker ends with its first producer.
    let mut persist = false;
    // Encode at most this many frames a second, whatever the producer sends.
    let mut max_fps: Option<u32> = None;
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        let text = arg.to_str().context("non-UTF-8 argument")?;
        if text == "--persist" {
            persist = true;
        } else if text == "--max-fps" {
            let value: u32 = rest
                .next()
                .context("--max-fps needs a value")?
                .to_str()
                .context("non-UTF-8 option value")?
                .parse()
                .context("--max-fps")?;
            ensure!((1..=240).contains(&value), "--max-fps out of range");
            max_fps = Some(value);
        } else if matches!(text, "--iroh-local" | "--iroh-share") {
            ensure!(
                transport == publish::Transport::Browser,
                "choose one of --iroh-local and --iroh-share"
            );
            transport = if text == "--iroh-local" {
                publish::Transport::Loopback
            } else {
                publish::Transport::Peer
            };
        } else if text == "--keyframe-seconds" {
            keyframe_seconds = rest
                .next()
                .context("--keyframe-seconds needs a value")?
                .to_str()
                .context("non-UTF-8 option value")?
                .parse()
                .context("--keyframe-seconds")?;
            ensure!((1..=60).contains(&keyframe_seconds), "--keyframe-seconds out of range");
        } else if matches!(text, "--pose-ring" | "--pose-lag-us") {
            let value = rest
                .next()
                .with_context(|| format!("{text} needs a value"))?
                .to_str()
                .context("non-UTF-8 option value")?;
            if text == "--pose-ring" {
                pose_path = Some(value.into());
            } else {
                pose_lag_us = value.parse().context("--pose-lag-us")?;
                ensure!(pose_lag_us <= 100_000, "--pose-lag-us out of range");
            }
        } else if matches!(text, "--fps" | "--bitrate" | "--layout") {
            let value = rest
                .next()
                .with_context(|| format!("{text} needs a value"))?
                .to_str()
                .context("non-UTF-8 option value")?;
            match text {
                "--fps" => framerate = value.parse().context("--fps")?,
                "--bitrate" => bitrate = value.parse().context("--bitrate")?,
                _ => layout = publish::Layout::parse(value)?,
            }
        } else if text.starts_with("--") {
            bail!("unknown media option {text}; {usage}");
        } else {
            positional.push(arg.as_os_str());
        }
    }
    ensure!((2..=3).contains(&positional.len()), "{usage}");
    ensure!(
        (1..=240).contains(&framerate) && (100_000..=200_000_000).contains(&bitrate),
        "framerate or bitrate out of range"
    );
    // Each keyframe starts a MoQ group, and moq-net aborts a group past 32 MB of cached frames.
    // Keep a group within 3/4 of that so VBR peaks and keyframes fit.
    ensure!(
        u64::from(bitrate) / 8 * u64::from(keyframe_seconds) <= 24 * 1024 * 1024,
        "--keyframe-seconds {keyframe_seconds} at {bitrate} bit/s exceeds the 32 MB MoQ group limit"
    );
    let socket = Path::new(positional[0]);
    let output_path = Path::new(positional[1]);
    let recording = output_path != Path::new("-");
    let mut publisher = positional
        .get(2)
        .map(|bind| -> Result<_> {
            let address = bind.to_str().context("invalid listen address")?.parse()?;
            let publisher = publish::Publisher::new(address, transport)?;
            web::start(address, &publisher.fingerprint)?;
            println!(
                "MOQ_READY listen={} fingerprint={}",
                bind.to_string_lossy(),
                publisher.fingerprint
            );
            if recording {
                fs::write(
                    output_path.with_extension("certificate.sha256"),
                    &publisher.fingerprint,
                )?;
            }
            Ok(publisher)
        })
        .transpose()?;
    let (listener, _path) = listen(socket)?;
    println!("MEDIA_READY socket={}", socket.display());
    let mut output = recording.then(|| File::create(output_path)).transpose()?;
    let mut journal = recording
        .then(|| File::create(output_path.with_extension("frames.jsonl")))
        .transpose()?;
    // PETPLAY_FRAME_TRACE=PATH: one line per published frame, for pacing analysis.
    let mut frame_trace = std::env::var_os("PETPLAY_FRAME_TRACE")
        .map(|path| File::create(path).map(std::io::LineWriter::new))
        .transpose()?;
    let mut encoder: Option<Encoder> = None;
    let mut geometry = None;
    let mut last: [u64; 3];
    let min_interval = max_fps.map(|fps| Duration::from_secs_f64(0.75 / f64::from(fps)));
    let mut last_encoded: Option<Instant> = None;
    let mut skipped = 0u64;
    let mut frames = 0u64;
    let mut position = 0u64;
    let mut pose_ring: Option<pose_ring::PoseRing> = None;
    let mut pose_retry = Instant::now();
    let mut tagged = 0u64;
    // Per second of video, in microseconds: where captures fall after the display tick, and
    // the head pose's path from arrival at the driver to publication of its frame.
    let mut timing: [Vec<u64>; 4] = Default::default();
    let monotonic = || {
        let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
        t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
    };
    loop {
        let client = owned(syscall(
            unsafe {
                libc::accept4(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            },
            "accept producer",
        )?);
        // Each producer numbers its own slots from the start; its first picture is a keyframe so
        // receivers recover at once.
        last = [0u64; 3];
        let mut force_key = true;
        loop {
            let (b, mut fds) = match receive(&client) {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(error) if persist => {
                    println!("MEDIA_PRODUCER_LOST {error:#}");
                    break;
                }
                Err(error) => return Err(error),
            };
            let slot = u32at(&b, 8) as usize;
            let seq = u64at(&b, 16);
            let timestamp = u64at(&b, 24);
            let width = u32at(&b, 32);
            let height = u32at(&b, 36);
            let pitch = u32at(&b, 40);
            let offset = u32at(&b, 44);
            let modifier = u64at(&b, 48);
            let format = u32at(&b, 56);
            ensure!(
                slot < 3
                    && seq > last[slot]
                    && (16..=8192).contains(&width)
                    && (16..=8192).contains(&height)
                    && width % 2 == 0
                    && height % 2 == 0
                    && pitch >= width * 4
                    && pitch <= 65536
                    && offset <= 65536
                    && modifier == 0
                    && format == 44,
                "unsupported frame descriptor or sequence"
            );
            // --max-fps: a producer faster than the cap has the extra frames released unread
            // (no fence wait, no encode). The 3/4 margin lets a source at the cap through its
            // jitter, while a much faster one is held to about the cap.
            if let Some(interval) = min_interval {
                let now = Instant::now();
                if last_encoded.is_some_and(|at| now.duration_since(at) < interval) {
                    last[slot] = seq;
                    skipped += 1;
                    if let Err(error) = release(&client, slot, seq) {
                        if persist {
                            println!("MEDIA_PRODUCER_LOST slot release failed: {error}");
                            break;
                        }
                        bail!("slot release failed: {error}");
                    }
                    continue;
                }
                last_encoded = Some(now);
            }
            if u32at(&b, 12) == 0 {
                let ready = fds.pop().unwrap();
                wait_ready(&ready)?;
            }
            let image = fds.pop().unwrap();
            if encoder.is_none() {
                let mut cfg = Config::new(width, height, framerate, bitrate, framerate * keyframe_seconds);
                cfg.device = Some("/dev/dri/renderD128".into());
                encoder = Some(Encoder::new(cfg).context("hardware H.264 encoder")?);
                geometry = Some((width, height));
                if let Some(publisher) = publisher.as_mut() {
                    publisher.describe(publish::Format {
                        width,
                        height,
                        framerate,
                        layout,
                    });
                }
                println!(
                    "MEDIA_SOURCE width={width} height={height} fps={framerate} layout={layout:?}"
                );
            }
            ensure!(
                geometry == Some((width, height)),
                "stream resize requires a new session"
            );
            let start = Instant::now();
            let picked_up = monotonic();
            let key = force_key || frames % u64::from(framerate * keyframe_seconds) == 0;
            force_key = false;
            let buffer = DmaBuf {
                drm_format: drm::ARGB8888,
                modifier,
                width,
                height,
                planes: vec![Plane { offset, pitch }],
                fd: image,
                color: None,
            };
            let encoded = encoder
                .as_mut()
                .unwrap()
                .encode_dmabuf(buffer, key)
                .context("strict GPU DMA-BUF encode")?;
            ensure!(!encoded.is_empty(), "empty encoded frame");
            let encoded_at = monotonic();
            if let Some(publisher) = publisher.as_mut() {
                publisher.frame(&encoded, key, timestamp)?;
                if let Some(trace) = frame_trace.as_mut() {
                    // Capture time and µs after it: picked up, encoded, handed to MoQ.
                    writeln!(
                        trace,
                        "{timestamp} {} {} {} {} {key}",
                        picked_up.saturating_sub(timestamp) / 1000,
                        encoded_at.saturating_sub(timestamp) / 1000,
                        monotonic().saturating_sub(timestamp) / 1000,
                        encoded.len(),
                    )?;
                }
                // The driver creates the ring when SteamVR starts, usually after this worker.
                if let Some(path) = pose_path.as_ref().filter(|_| pose_ring.is_none()) {
                    if pose_retry.elapsed() > Duration::from_secs(1) || frames == 0 {
                        pose_retry = Instant::now();
                        match pose_ring::PoseRing::open(path) {
                            Ok(ring) => {
                                println!("POSE_RING_READY path={}", path.display());
                                pose_ring = Some(ring);
                            }
                            Err(err) if frames % u64::from(framerate) == 0 => {
                                println!("POSE_RING_WAITING {err:#}")
                            }
                            Err(_) => {}
                        }
                    }
                }
                let sampled = timestamp.saturating_sub(pose_lag_us * 1000);
                if let Some(pose) = pose_ring.as_ref().and_then(|ring| ring.at(sampled)) {
                    let age = ((timestamp - pose.time_ns.min(timestamp)) / 1000).min(u64::from(u32::MAX));
                    publisher.pose(timestamp, &pose, age as u32)?;
                    tagged += 1;
                    let hz = u128::from(framerate);
                    let phase = ((u128::from(timestamp) * hz % 1_000_000_000) / hz / 1000) as u64;
                    let published = monotonic();
                    for (series, value) in timing.iter_mut().zip([
                        phase,
                        pose.time_ns.saturating_sub(pose.held_ns) / 1000,
                        age,
                        published.saturating_sub(timestamp) / 1000,
                    ]) {
                        series.push(value);
                    }
                    if tagged % u64::from(framerate) == 0 {
                        let summary = |v: &mut Vec<u64>| {
                            v.sort_unstable();
                            let text = format!("{}/{}", v[v.len() / 2], v[v.len() - 1]);
                            v.clear();
                            text
                        };
                        let [phase, wait, latch, publish] = &mut timing;
                        println!(
                            "POSE_TIMING_US median/max capture_after_tick={} arrival_to_latch={} latch_to_capture={} capture_to_published={} sequence={}",
                            summary(phase), summary(wait), summary(latch), summary(publish), pose.client_sequence
                        );
                    }
                }
            }
            if let Some(output) = output.as_mut() {
                output.write_all(&encoded)?;
            }
            if let Some(journal) = journal.as_mut() {
                writeln!(
                    journal,
                    "{{\"sequence\":{seq},\"timestamp_ns\":{timestamp},\"offset\":{position},\"bytes\":{},\"keyframe\":{key},\"encode_ms\":{:.3}}}",
                    encoded.len(),
                    start.elapsed().as_secs_f64() * 1000.
                )?;
            }
            position += encoded.len() as u64;
            last[slot] = seq;
            frames += 1;
            // encode_dmabuf returns only after VA has finished reading the imported image.
            // Returning this exact slot/sequence is the producer's reuse permission.
            if let Err(error) = release(&client, slot, seq) {
                if persist {
                    println!("MEDIA_PRODUCER_LOST slot release failed: {error}");
                    break;
                }
                bail!("slot release failed: {error}");
            }
            if frames <= 3 || frames % u64::from(framerate) == 0 {
                println!(
                    "MEDIA_ENCODED frames={frames} sequence={seq} bytes={} raw_cpu_bytes=0 skipped={skipped}",
                    encoded.len()
                );
            }
        }
        if !persist {
            break;
        }
        println!("MEDIA_PRODUCER_DONE frames={frames}");
    }
    println!("MEDIA_DONE frames={frames} encoded_bytes={position}");
    Ok(())
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    fn packet(bytes: &[u8], rights: usize) -> Result<Option<([u8; FRAME], Vec<OwnedFd>)>> {
        let mut pair = [-1; 2];
        syscall(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            "socketpair",
        )?;
        let sender = owned(pair[0]);
        let receiver = owned(pair[1]);
        let file = File::open("/dev/null")?;
        let mut io = libc::iovec {
            iov_base: bytes.as_ptr() as *mut _,
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = &mut io;
        msg.msg_iovlen = 1;
        if rights > 0 {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen =
                unsafe { libc::CMSG_SPACE((rights * size_of::<i32>()) as u32) } as usize;
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_len = libc::CMSG_LEN((rights * size_of::<i32>()) as u32) as usize;
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                for i in 0..rights {
                    *libc::CMSG_DATA(c).cast::<i32>().add(i) = file.as_raw_fd();
                }
            }
        }
        let n = unsafe { libc::sendmsg(sender.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        ensure!(n == bytes.len() as isize, "send test packet");
        receive(&receiver)
    }
    #[test]
    fn descriptor_messages_enforce_size_version_and_fence_fd_ownership() {
        let mut b = [0u8; FRAME];
        b[..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(packet(&b, 2).unwrap().unwrap().1.len(), 2);
        assert!(packet(&b, 1).is_err());
        b[12..16].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(packet(&b, 1).unwrap().unwrap().1.len(), 1);
        assert!(packet(&b, 2).is_err());
        assert!(packet(&b[..63], 1).is_err());
        let mut oversized = b.to_vec();
        oversized.push(0);
        assert!(packet(&oversized, 1).is_err());
        assert!(packet(&[], 1).is_err());
        b[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert!(packet(&b, 1).is_err());
    }
}
