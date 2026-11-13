//! Native media worker. Only DMA-BUF descriptors and encoded bytes cross CPU memory.
mod gateway;
mod publish;
mod web;
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
    time::Instant,
};
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
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "gateway") {
        return gateway::run(&args[2..]);
    }
    let usage = "usage: petplay-media SOCKET OUTPUT.h264 [QUIC_LISTEN_ADDR] \
        [--iroh-local] [--fps N] [--bitrate BITS_PER_SECOND] [--layout mono|side-by-side]";
    let mut positional: Vec<&std::ffi::OsStr> = Vec::new();
    let mut iroh_local = false;
    let mut framerate = 75u32;
    let mut bitrate = 12_000_000u32;
    let mut layout = publish::Layout::SideBySide;
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        let text = arg.to_str().context("non-UTF-8 argument")?;
        if text == "--iroh-local" {
            iroh_local = true;
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
    let socket = Path::new(positional[0]);
    let output_path = Path::new(positional[1]);
    let recording = output_path != Path::new("-");
    let mut publisher = positional
        .get(2)
        .map(|bind| -> Result<_> {
            let address = bind.to_str().context("invalid listen address")?.parse()?;
            let publisher = publish::Publisher::new(address, iroh_local)?;
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
    let mut output = recording.then(|| File::create(output_path)).transpose()?;
    let mut journal = recording
        .then(|| File::create(output_path.with_extension("frames.jsonl")))
        .transpose()?;
    let mut encoder: Option<Encoder> = None;
    let mut geometry = None;
    let mut last = [0u64; 3];
    let mut frames = 0u64;
    let mut position = 0u64;
    while let Some((b, mut fds)) = receive(&client)? {
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
        if u32at(&b, 12) == 0 {
            let ready = fds.pop().unwrap();
            wait_ready(&ready)?;
        }
        let image = fds.pop().unwrap();
        if encoder.is_none() {
            let mut cfg = Config::new(width, height, framerate, bitrate, framerate);
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
        let key = frames % u64::from(framerate) == 0;
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
        if let Some(publisher) = publisher.as_mut() {
            publisher.frame(&encoded, key, timestamp)?;
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
            bail!("slot release failed: {}", std::io::Error::last_os_error());
        }
        if frames <= 3 || frames % u64::from(framerate) == 0 {
            println!(
                "MEDIA_ENCODED frames={frames} sequence={seq} bytes={} raw_cpu_bytes=0",
                encoded.len()
            );
        }
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
