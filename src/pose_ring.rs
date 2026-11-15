//! Reader for the driver's held-head-pose ring (`hmd-poses` beside its control socket).
//! The driver is the single writer; each 64-byte entry is guarded by a seqlock.
use anyhow::{Context, Result, ensure};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering, fence};

const MAGIC: u32 = 0x5050_4850;
const CAPACITY: u64 = 64;
const ENTRY: usize = 64;
const SIZE: usize = 64 + CAPACITY as usize * ENTRY;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub time_ns: u64,
    pub client_sequence: u64,
    pub rotation: [f32; 4],
    pub position: [f32; 3],
    /// When the driver received the pose; `time_ns` is when it was latched for a frame.
    pub held_ns: u64,
}

pub struct PoseRing {
    base: *const u8,
    len: usize,
}
// The mapping is read-only and only read through atomics/volatile loads.
unsafe impl Send for PoseRing {}

impl PoseRing {
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        let len = file.metadata()?.len() as usize;
        ensure!(len >= SIZE, "pose ring too small");
        let base = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0)
        };
        ensure!(base != libc::MAP_FAILED, "mmap pose ring");
        let ring = Self { base: base as *const u8, len };
        ensure!(
            ring.u32(0) == MAGIC && ring.u32(4) == 1 && ring.u32(8) == CAPACITY as u32 && ring.u32(12) == ENTRY as u32,
            "not a version-1 pose ring"
        );
        Ok(ring)
    }
    fn u32(&self, offset: usize) -> u32 {
        unsafe { std::ptr::read_volatile(self.base.add(offset) as *const u32) }
    }
    fn u64(&self, offset: usize) -> u64 {
        unsafe { std::ptr::read_volatile(self.base.add(offset) as *const u64) }
    }
    fn f32(&self, offset: usize) -> f32 {
        f32::from_bits(self.u32(offset))
    }
    fn atomic(&self, offset: usize) -> &AtomicU64 {
        unsafe { &*(self.base.add(offset) as *const AtomicU64) }
    }
    fn entry(&self, index: u64) -> Option<Pose> {
        let base = 64 + (index % CAPACITY) as usize * ENTRY;
        for _ in 0..4 {
            let before = self.atomic(base).load(Ordering::Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let pose = Pose {
                time_ns: self.u64(base + 8),
                client_sequence: self.u64(base + 16),
                rotation: std::array::from_fn(|i| self.f32(base + 24 + i * 4)),
                position: std::array::from_fn(|i| self.f32(base + 40 + i * 4)),
                held_ns: self.u64(base + 56),
            };
            fence(Ordering::Acquire);
            if self.atomic(base).load(Ordering::Relaxed) == before {
                return Some(pose);
            }
        }
        None
    }
    /// The newest pose held at or before `time_ns` (CLOCK_MONOTONIC), if still in the ring.
    pub fn at(&self, time_ns: u64) -> Option<Pose> {
        let count = self.atomic(16).load(Ordering::Acquire);
        (count.saturating_sub(CAPACITY)..count)
            .rev()
            .filter_map(|i| self.entry(i))
            .find(|pose| pose.time_ns <= time_ns)
    }
}
impl Drop for PoseRing {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn write(bytes: &mut [u8], index: usize, time: u64, seq: u64, lock: u64) {
        let b = 64 + index * ENTRY;
        bytes[b..b + 8].copy_from_slice(&lock.to_le_bytes());
        bytes[b + 8..b + 16].copy_from_slice(&time.to_le_bytes());
        bytes[b + 16..b + 24].copy_from_slice(&seq.to_le_bytes());
        bytes[b + 28..b + 32].copy_from_slice(&0.5f32.to_le_bytes());
        bytes[b + 48..b + 52].copy_from_slice(&3f32.to_le_bytes());
    }
    #[test]
    fn finds_the_newest_pose_held_before_a_capture_and_skips_torn_entries() {
        let mut bytes = vec![0u8; 8192];
        bytes[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&64u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&64u32.to_le_bytes());
        bytes[16..24].copy_from_slice(&3u64.to_le_bytes());
        write(&mut bytes, 0, 100, 7, 2);
        write(&mut bytes, 1, 200, 8, 2);
        write(&mut bytes, 2, 300, 9, 3); // being written
        let dir = std::env::temp_dir().join(format!("pose-ring-test-{}", std::process::id()));
        std::fs::File::create(&dir).unwrap().write_all(&bytes).unwrap();
        let ring = PoseRing::open(&dir).unwrap();
        assert_eq!(ring.at(250).unwrap().client_sequence, 8);
        assert_eq!(ring.at(350).unwrap().client_sequence, 8, "torn entry is skipped");
        assert_eq!(ring.at(150).unwrap().rotation[1], 0.5);
        assert_eq!(ring.at(150).unwrap().position[2], 3.0);
        assert!(ring.at(50).is_none());
        std::fs::remove_file(dir).unwrap();
    }
}
