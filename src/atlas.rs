//! Per-machine GPU Atlas encoder (`petplay-media atlas`).
//!
//! Composites multiple DMA-BUF window producers (`screen-streamer --window=<id>`) directly onto
//! one persistent VA-API NV12 surface via hardware VPP region blits (`blit_dmabuf_region`), then
//! encodes and publishes a single H.264 stream over WebTransport + WebRTC.
//!
//! Stdin line protocol:
//!   ADD <id> <socket_path>
//!   REMOVE <id>
//!
//! Stdout events:
//!   MOQ_READY ... / WEBRTC_READY ... / MEDIA_READY
//!   ATLAS_LAYOUT {"width":2560,"height":1440,"slots":{"<id>":{"srcW":...,"srcH":...,"crop":{"x":...,"y":...,"width":...,"height":...}}}}
use anyhow::{Context, Result, bail, ensure};
use moq_vaapi::{
    dmabuf::{DmaBuf, Plane, drm},
    encode::{Config, Encoder},
};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::{self, BufRead},
    mem::{self, size_of},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::publish;

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
fn monotonic_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

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

fn wait_ready(fd: &OwnedFd) -> Result<()> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let n = syscall(unsafe { libc::poll(&mut pfd, 1, 500) }, "poll sync fd")?;
    ensure!(n == 1 && (pfd.revents & libc::POLLIN) != 0, "sync fd timeout");
    Ok(())
}

pub struct SocketPath {
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

pub fn bind_slot_socket(path: &Path) -> Result<(OwnedFd, SocketPath)> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    listen_seqpacket(path)
}

fn listen_seqpacket(path: &Path) -> Result<(OwnedFd, SocketPath)> {
    use std::os::unix::ffi::OsStrExt;
    let _ = fs::remove_file(path);
    let bytes = path.as_os_str().as_bytes();
    ensure!(
        !bytes.contains(&0) && bytes.len() < 108,
        "socket path invalid"
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
    syscall(unsafe { libc::listen(fd.as_raw_fd(), 2) }, "listen")?;
    Ok((fd, guard))
}

fn receive_frame(fd: &OwnedFd) -> Result<Option<([u8; FRAME], Vec<OwnedFd>)>> {
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
    ensure!(n as usize == FRAME && u32at(&bytes, 0) == MAGIC, "bad frame header");
    Ok(Some((bytes, fds)))
}

struct PendingFrame {
    slot: usize,
    seq: u64,
    width: u32,
    height: u32,
    pitch: u32,
    offset: u32,
    modifier: u64,
    image: OwnedFd,
    client: Arc<OwnedFd>,
}

struct SlotHandle {
    id: String,
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<PendingFrame>>>,
    src_w: u32,
    src_h: u32,
    rect: (i16, i16, u16, u16),
}

pub enum AtlasCmd {
    Add { id: String, socket_path: PathBuf },
    AddBound { id: String, listener: OwnedFd, guard: SocketPath },
    Remove { id: String },
    Quit,
}

fn spawn_slot_reader(
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<PendingFrame>>>,
) -> Result<()> {
    let (listener, guard) = listen_seqpacket(&socket_path)?;
    spawn_slot_reader_bound(listener, guard, stop, latest);
    Ok(())
}

fn spawn_slot_reader_bound(
    listener: OwnedFd,
    guard: SocketPath,
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<PendingFrame>>>,
) {
    thread::spawn(move || {
        let _guard = guard;
        while !stop.load(Ordering::Relaxed) {
            let mut pfd = libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
            if ready <= 0 {
                continue;
            }
            let raw_client = unsafe {
                libc::accept4(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if raw_client < 0 {
                continue;
            }
            let client = Arc::new(owned(raw_client));
            while !stop.load(Ordering::Relaxed) {
                let mut cpfd = libc::pollfd {
                    fd: client.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let cready = unsafe { libc::poll(&mut cpfd, 1, 100) };
                if cready < 0 {
                    break;
                }
                if cready == 0 {
                    continue;
                }
                let Ok(Some((b, mut fds))) = receive_frame(&client) else {
                    break;
                };
                let slot = u32at(&b, 8) as usize;
                let flags = u32at(&b, 12);
                let seq = u64at(&b, 16);
                let width = u32at(&b, 32);
                let height = u32at(&b, 36);
                let pitch = u32at(&b, 40);
                let offset = u32at(&b, 44);
                let modifier = u64at(&b, 48);
                let format = u32at(&b, 56);
                if slot >= 3
                    || !(16..=8192).contains(&width)
                    || !(16..=8192).contains(&height)
                    || pitch < width * 4
                    || modifier != 0
                    || format != 44
                {
                    let _ = release(&client, slot, seq);
                    continue;
                }
                if flags == 0 {
                    if let Some(ready_fd) = fds.pop() {
                        if wait_ready(&ready_fd).is_err() {
                            let _ = release(&client, slot, seq);
                            continue;
                        }
                    }
                }
                let Some(image) = fds.pop() else {
                    let _ = release(&client, slot, seq);
                    continue;
                };
                let next = PendingFrame {
                    slot,
                    seq,
                    width,
                    height,
                    pitch,
                    offset,
                    modifier,
                    image,
                    client: Arc::clone(&client),
                };
                // Replace any unread older frame and release its slot immediately so producer never stalls.
                let dropped = {
                    let mut guard = latest.lock().unwrap();
                    guard.replace(next)
                };
                if let Some(old) = dropped {
                    let _ = release(&old.client, old.slot, old.seq);
                }
            }
        }
    });
}

fn grid_dims(n: usize) -> (u32, u32) {
    match n {
        0 | 1 => (1, 1),
        2 => (2, 1),
        3..=4 => (2, 2),
        5..=6 => (3, 2),
        7..=9 => (3, 3),
        _ => (4, 3),
    }
}

fn recompute_layout(slots: &mut [SlotHandle], atlas_w: u32, atlas_h: u32) -> String {
    let (cols, rows) = grid_dims(slots.len());
    let cell_w = ((atlas_w / cols) & !1).max(16);
    let cell_h = ((atlas_h / rows) & !1).max(16);
    let mut map = serde_json::Map::new();
    for (idx, slot) in slots.iter_mut().enumerate() {
        let col = (idx as u32) % cols;
        let row = (idx as u32) / cols;
        let base_x = col * cell_w;
        let base_y = row * cell_h;
        let sw = slot.src_w.max(16) as f64;
        let sh = slot.src_h.max(16) as f64;
        // Preserve exact window aspect ratio inside the cell without upscaling past native pixels.
        let scale = (cell_w as f64 / sw).min(cell_h as f64 / sh).min(1.0);
        let rw = (((sw * scale).round() as u32) & !1).clamp(16, cell_w);
        let rh = (((sh * scale).round() as u32) & !1).clamp(16, cell_h);
        slot.rect = (base_x as i16, base_y as i16, rw as u16, rh as u16);
        map.insert(
            slot.id.clone(),
            serde_json::json!({
                "srcW": slot.src_w,
                "srcH": slot.src_h,
                "crop": {
                    "x": base_x as f64 / atlas_w as f64,
                    "y": base_y as f64 / atlas_h as f64,
                    "width": rw as f64 / atlas_w as f64,
                    "height": rh as f64 / atlas_h as f64,
                }
            }),
        );
    }
    serde_json::json!({
        "width": atlas_w,
        "height": atlas_h,
        "slots": map,
    })
    .to_string()
}

pub fn run(args: &[OsString]) -> Result<()> {
    let usage = "usage: petplay-media atlas <listen> [--width 2560] [--height 1440] [--fps 30] [--bitrate 12000000] [--iroh-share]";
    let listen = args
        .first()
        .and_then(|a| a.to_str())
        .context(usage)?
        .to_owned();
    let mut atlas_w: u32 = 2560;
    let mut atlas_h: u32 = 1440;
    let mut fps: u32 = 30;
    let mut bitrate: u32 = 12_000_000;
    let mut transport = publish::Transport::Browser;
    let mut i = 1;
    while i < args.len() {
        let flag = args[i].to_str().context(usage)?;
        if flag == "--iroh-share" {
            transport = publish::Transport::Peer;
            i += 1;
            continue;
        }
        let val = args.get(i + 1).and_then(|v| v.to_str()).context(usage)?;
        match flag {
            "--width" => atlas_w = val.parse()?,
            "--height" => atlas_h = val.parse()?,
            "--fps" => fps = val.parse()?,
            "--bitrate" => bitrate = val.parse()?,
            _ => bail!("{usage}"),
        }
        i += 2;
    }
    atlas_w = (atlas_w & !1).max(320);
    atlas_h = (atlas_h & !1).max(240);

    let bind: std::net::SocketAddr = listen.parse().context("parse listen address")?;
    let publisher = publish::Publisher::new(bind, transport)?;
    println!("MOQ_READY listen={bind} fingerprint={}", publisher.fingerprint);

    let (tx, rx) = mpsc::channel::<AtlasCmd>();
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let parts: Vec<&str> = line.trim().splitn(3, ' ').collect();
            match parts.as_slice() {
                ["ADD", id, path] if !id.is_empty() && !path.is_empty() => {
                    let _ = tx.send(AtlasCmd::Add {
                        id: (*id).to_owned(),
                        socket_path: PathBuf::from(*path),
                    });
                }
                ["REMOVE", id] if !id.is_empty() => {
                    let _ = tx.send(AtlasCmd::Remove { id: (*id).to_owned() });
                }
                _ => {}
            }
        }
        let _ = tx.send(AtlasCmd::Quit);
    });

    run_atlas_loop(
        atlas_w,
        atlas_h,
        fps,
        bitrate,
        publisher,
        rx,
        true,
        |layout_json| {
            println!("ATLAS_LAYOUT {layout_json}");
        },
    )
}

/// Shared GPU atlas composition loop used by both `petplay-media atlas` and `petplay-media actor`.
/// When `announce_stdout` is false (actor mode), nothing is written to stdout.
pub fn run_atlas_loop(
    atlas_w: u32,
    atlas_h: u32,
    fps: u32,
    bitrate: u32,
    mut publisher: publish::Publisher,
    rx: mpsc::Receiver<AtlasCmd>,
    announce_stdout: bool,
    mut on_layout: impl FnMut(&str),
) -> Result<()> {
    publisher.describe(publish::Format {
        width: atlas_w,
        height: atlas_h,
        framerate: fps,
        layout: publish::Layout::Mono,
    });

    let mut cfg = Config::new(atlas_w, atlas_h, fps, bitrate, fps * 2);
    cfg.device = Some("/dev/dri/renderD128".into());
    let mut encoder = Encoder::new(cfg).context("atlas VA-API H.264 encoder")?;
    let _ = encoder.clear_input_black();
    if announce_stdout {
        println!("MEDIA_READY");
        println!("MEDIA_SOURCE width={atlas_w} height={atlas_h} fps={fps} layout=Mono");
    }

    let mut slots: Vec<SlotHandle> = Vec::new();
    let mut index_by_id: HashMap<String, usize> = HashMap::new();
    let mut frame_count: u64 = 0;
    let mut has_content = false;
    let tick = Duration::from_secs_f64(1.0 / (fps.max(1) as f64));

    loop {
        let loop_start = Instant::now();
        let mut layout_dirty = false;
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                AtlasCmd::Add { id, socket_path } => {
                    if index_by_id.contains_key(&id) {
                        continue;
                    }
                    let stop = Arc::new(AtomicBool::new(false));
                    let latest = Arc::new(Mutex::new(None));
                    if let Err(err) = spawn_slot_reader(socket_path, Arc::clone(&stop), Arc::clone(&latest)) {
                        eprintln!("ATLAS_SLOT_ERROR {id}: {err:#}");
                        continue;
                    }
                    index_by_id.insert(id.clone(), slots.len());
                    slots.push(SlotHandle {
                        id,
                        stop,
                        latest,
                        src_w: 1280,
                        src_h: 720,
                        rect: (0, 0, 1280, 720),
                    });
                    layout_dirty = true;
                }
                AtlasCmd::AddBound { id, listener, guard } => {
                    if index_by_id.contains_key(&id) {
                        continue;
                    }
                    let stop = Arc::new(AtomicBool::new(false));
                    let latest = Arc::new(Mutex::new(None));
                    spawn_slot_reader_bound(listener, guard, Arc::clone(&stop), Arc::clone(&latest));
                    index_by_id.insert(id.clone(), slots.len());
                    slots.push(SlotHandle {
                        id,
                        stop,
                        latest,
                        src_w: 1280,
                        src_h: 720,
                        rect: (0, 0, 1280, 720),
                    });
                    layout_dirty = true;
                }
                AtlasCmd::Remove { id } => {
                    if let Some(pos) = index_by_id.remove(&id) {
                        let removed = slots.remove(pos);
                        removed.stop.store(true, Ordering::Relaxed);
                        index_by_id.clear();
                        for (idx, s) in slots.iter().enumerate() {
                            index_by_id.insert(s.id.clone(), idx);
                        }
                        layout_dirty = true;
                    }
                }
                AtlasCmd::Quit => {
                    for s in &slots {
                        s.stop.store(true, Ordering::Relaxed);
                    }
                    return Ok(());
                }
            }
        }

        // First pass: check if any incoming frame changed its window geometry.
        let mut pending: Vec<(usize, PendingFrame)> = Vec::new();
        for (idx, slot) in slots.iter_mut().enumerate() {
            let taken = slot.latest.lock().unwrap().take();
            if let Some(frame) = taken {
                if frame.width != slot.src_w || frame.height != slot.src_h {
                    slot.src_w = frame.width;
                    slot.src_h = frame.height;
                    layout_dirty = true;
                }
                pending.push((idx, frame));
            }
        }

        if layout_dirty {
            let _ = encoder.clear_input_black();
            let json = recompute_layout(&mut slots, atlas_w, atlas_h);
            on_layout(&json);
        }

        // Blit every updated slot into the persistent NV12 atlas surface.
        let mut blitted = false;
        for (idx, frame) in pending {
            let rect = slots[idx].rect;
            let buffer = DmaBuf {
                drm_format: drm::ARGB8888,
                modifier: frame.modifier,
                width: frame.width,
                height: frame.height,
                planes: vec![Plane {
                    offset: frame.offset,
                    pitch: frame.pitch,
                }],
                fd: frame.image,
                color: None,
            };
            if let Err(err) = encoder.blit_dmabuf_region(buffer, rect) {
                eprintln!("ATLAS_BLIT_ERROR {}: {err:#}", slots[idx].id);
            } else {
                blitted = true;
                has_content = true;
            }
            let _ = release(&frame.client, frame.slot, frame.seq);
        }

        if has_content && (blitted || frame_count % u64::from(fps) == 0) {
            let key = frame_count % u64::from(fps * 2) == 0 || layout_dirty;
            match encoder.encode_input(key) {
                Ok(encoded) if !encoded.is_empty() => {
                    let _ = publisher.frame(&encoded, key, monotonic_ns());
                    frame_count += 1;
                }
                Ok(_) => {}
                Err(err) => eprintln!("ATLAS_ENCODE_ERROR: {err:#}"),
            }
        }

        let elapsed = loop_start.elapsed();
        if elapsed < tick {
            thread::sleep(tick - elapsed);
        }
    }
}
