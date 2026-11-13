//! GPU-only receiver output. PPM1 RGB descriptors, exact slot ACKs, no pixel mappings.
use anyhow::{Context, Result, ensure};
use gbm::{BufferObject, BufferObjectFlags, Device, Format, Modifier};
use moq_vaapi::{
    Surface,
    dmabuf::{DmaBuf, Plane, drm},
    vpp::Processor,
};
use std::{
    fs::{File, OpenOptions},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
};
const MAGIC: u32 = 0x50504d31;
fn check(n: i32) -> Result<i32> {
    if n < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(n)
    }
}
pub struct Output {
    socket: OwnedFd,
    _path: SocketPath,
    slots: Vec<Slot>,
    device: Device<File>,
    sequence: u64,
    pub sent: u64,
    pub dropped: u64,
}
struct Slot {
    buffer: BufferObject<()>,
    surface: Surface<DmaBuf>,
    pitch: u32,
    pending: Option<u64>,
}
impl Output {
    pub fn bind(path: &Path) -> Result<Self> {
        ensure!(
            !path.exists()
                && path.as_os_str().as_bytes().len() < 108
                && !path.as_os_str().as_bytes().contains(&0),
            "receiver socket exists or path is too long"
        );
        let listener = unsafe {
            OwnedFd::from_raw_fd(check(libc::socket(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
            ))?)
        };
        let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
        address.sun_family = libc::AF_UNIX as _;
        for (i, b) in path.as_os_str().as_bytes().iter().enumerate() {
            address.sun_path[i] = *b as _;
        }
        check(unsafe {
            libc::bind(
                listener.as_raw_fd(),
                &address as *const _ as *const _,
                mem::size_of_val(&address) as _,
            )
        })?;
        let path_guard = SocketPath {
            path: path.into(),
            inode: std::fs::symlink_metadata(path)?.ino(),
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        check(unsafe { libc::listen(listener.as_raw_fd(), 1) })?;
        println!("RECEIVER_READY socket={}", path.display());
        let socket = unsafe {
            OwnedFd::from_raw_fd(check(libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            ))?)
        };
        let node = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/dri/renderD128")?;
        let device = Device::new(node)?;
        Ok(Self {
            socket,
            _path: path_guard,
            slots: Vec::new(),
            device,
            sequence: 0,
            sent: 0,
            dropped: 0,
        })
    }
    pub fn collect(&mut self) -> Result<bool> {
        loop {
            let mut ack = [0u8; 16];
            let n = unsafe {
                libc::recv(
                    self.socket.as_raw_fd(),
                    ack.as_mut_ptr().cast(),
                    ack.len(),
                    libc::MSG_DONTWAIT | libc::MSG_TRUNC,
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::ConnectionReset {
                    return Ok(false);
                }
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(true);
                }
                return Err(e.into());
            }
            if n == 0 {
                return Ok(false);
            }
            ensure!(
                n == 16 && u32::from_le_bytes(ack[..4].try_into()?) == MAGIC,
                "invalid receiver frame ACK"
            );
            let index = u32::from_le_bytes(ack[4..8].try_into()?) as usize;
            let sequence = u64::from_le_bytes(ack[8..].try_into()?);
            let slot = self
                .slots
                .get_mut(index)
                .context("ACK for an unknown slot")?;
            ensure!(
                slot.pending == Some(sequence),
                "ACK does not match the pending frame"
            );
            slot.pending = None;
        }
    }
    pub fn present(
        &mut self,
        processor: &Processor,
        input: &Surface<DmaBuf>,
        width: u32,
        height: u32,
        timestamp: u64,
    ) -> Result<()> {
        if self.slots.is_empty() {
            for _ in 0..3 {
                let buffer = self.device.create_buffer_object_with_modifiers2::<()>(
                    width,
                    height,
                    Format::Argb8888,
                    [Modifier::Linear].into_iter(),
                    BufferObjectFlags::RENDERING,
                )?;
                ensure!(
                    buffer.modifier() == Modifier::Linear
                        && buffer.plane_count() == 1
                        && buffer.offset(0) == 0,
                    "unsupported receiver buffer layout"
                );
                let pitch = buffer.stride();
                let surface = processor.import(DmaBuf {
                    drm_format: drm::ARGB8888,
                    modifier: 0,
                    width,
                    height,
                    planes: vec![Plane { offset: 0, pitch }],
                    fd: buffer.fd()?,
                    color: None,
                })?;
                self.slots.push(Slot {
                    buffer,
                    surface,
                    pitch,
                    pending: None,
                });
            }
        }
        let Some(index) = self.slots.iter().position(|s| s.pending.is_none()) else {
            self.dropped += 1;
            return Ok(());
        };
        let slot = &mut self.slots[index];
        processor
            .blit(
                input,
                &slot.surface,
                Some(moq_vaapi::Color::infer(height)),
                None,
            )
            .context("GPU NV12-to-RGB conversion")?;
        self.sequence += 1;
        let mut header = [0u8; 64];
        for (offset, value) in [
            (0, MAGIC),
            (4, 1),
            (8, index as u32),
            (12, 1),
            (32, width),
            (36, height),
            (40, slot.pitch),
            (56, 44),
        ] {
            header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        header[16..24].copy_from_slice(&self.sequence.to_le_bytes());
        header[24..32].copy_from_slice(&timestamp.saturating_mul(1000).to_le_bytes());
        let fd = slot.buffer.fd()?;
        let mut io = libc::iovec {
            iov_base: header.as_mut_ptr().cast(),
            iov_len: header.len(),
        };
        let mut ancillary = [0usize; 4];
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = &mut io;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as usize;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&message);
            (*c).cmsg_len = libc::CMSG_LEN(4) as usize;
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            *libc::CMSG_DATA(c).cast::<i32>() = fd.as_raw_fd();
        }
        let n = unsafe {
            libc::sendmsg(
                self.socket.as_raw_fd(),
                &message,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                self.dropped += 1;
                return Ok(());
            }
            if matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ) {
                return Ok(());
            }
            return Err(e.into());
        }
        ensure!(n == 64, "short receiver frame send");
        slot.pending = Some(self.sequence);
        self.sent += 1;
        Ok(())
    }
}
struct SocketPath {
    path: PathBuf,
    inode: u64,
}
impl Drop for SocketPath {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.inode) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
