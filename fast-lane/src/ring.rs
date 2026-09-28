//! Reader of the shredstream proxy's shared-memory ring **v2** (phase 2 input).
//!
//! Layout and protocol are defined by the proxy (`proxy/src/shmem_ring_v2.rs`, branch
//! `fl-ring-v2` of shredstream-proxy); constants here must match. Each record carries a
//! bincode `Vec<Entry>` plus its ledger position: slot, parent slot, the batch's first data
//! shred index, the last shred index covered, the index of its first entry within the
//! batch, and FINAL / LAST_IN_SLOT / GUESSED_START flags. Seqlock read: copy, then re-check
//! that the writer cannot have reached the copied bytes.

use std::{
    fs::OpenOptions,
    io,
    os::unix::io::AsRawFd,
    path::Path,
    ptr,
    sync::atomic::{AtomicU64, Ordering, fence},
};

pub const MAGIC_V2: u64 = u64::from_le_bytes(*b"SHRINGv2");
pub const HEADER_SIZE: usize = 128;
pub const RECORD_HEADER_SIZE: usize = 64;
pub const FLAG_FINAL: u16 = 1;
pub const FLAG_LAST_IN_SLOT: u16 = 2;
pub const FLAG_GUESSED_START: u16 = 4;

// Header field offsets (see the proxy's `RingHeaderV2`).
const OFF_MAGIC: usize = 0x00;
const OFF_DATA_REGION_SIZE: usize = 0x10;
const OFF_MAX_RECORD: usize = 0x18;
const OFF_GENERATION: usize = 0x28;
const OFF_WRITE_POS: usize = 0x40;
const OFF_WRITE_SEQ: usize = 0x48;

#[inline]
const fn align8(n: usize) -> usize {
    (n + 7) & !7
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordMeta {
    pub slot: u64,
    pub parent_slot: u64,
    pub batch_start: u32,
    pub batch_end: u32,
    pub entry_offset: u32,
    pub entry_count: u32,
    pub flags: u16,
    pub t_publish_ns: u64,
}

impl RecordMeta {
    pub fn is_final(&self) -> bool {
        self.flags & FLAG_FINAL != 0
    }
    pub fn last_in_slot(&self) -> bool {
        self.flags & FLAG_LAST_IN_SLOT != 0
    }
    pub fn parent(&self) -> Option<u64> {
        (self.parent_slot != u64::MAX).then_some(self.parent_slot)
    }
}

pub enum Poll {
    Record(RecordMeta, Vec<u8>),
    Empty,
    /// Lapped, or the producer restarted: positions were lost.
    Reset,
}

pub struct RingReader {
    mmap_ptr: *const u8,
    mmap_len: usize,
    data_region_size: usize,
    max_record: u64,
    read_pos: u64,
    read_seq: u64,
    generation: u64,
    pub resets: u64,
}

// SAFETY: one reader thread owns it.
unsafe impl Send for RingReader {}

impl RingReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let len = file.metadata()?.len() as usize;
        if len < HEADER_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "ring too small"));
        }
        // SAFETY: read-only shared mapping of a regular file; unmapped in Drop.
        let mmap_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if mmap_ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mmap_ptr = mmap_ptr as *const u8;
        let read_u64 = |off: usize| unsafe { ptr::read_volatile(mmap_ptr.add(off) as *const u64) };
        if read_u64(OFF_MAGIC) != MAGIC_V2 {
            unsafe { libc::munmap(mmap_ptr as *mut libc::c_void, len) };
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad ring v2 magic"));
        }
        fence(Ordering::Acquire);
        let data_region_size = read_u64(OFF_DATA_REGION_SIZE) as usize;
        if HEADER_SIZE + data_region_size > len || data_region_size == 0 {
            unsafe { libc::munmap(mmap_ptr as *mut libc::c_void, len) };
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad ring v2 size"));
        }
        let mut reader = Self {
            mmap_ptr,
            mmap_len: len,
            data_region_size,
            max_record: read_u64(OFF_MAX_RECORD),
            read_pos: 0,
            read_seq: 0,
            generation: read_u64(OFF_GENERATION),
            resets: 0,
        };
        reader.jump_to_head();
        Ok(reader)
    }

    fn atomic(&self, off: usize) -> &AtomicU64 {
        // SAFETY: 8-aligned offsets inside the mapping.
        unsafe { &*(self.mmap_ptr.add(off) as *const AtomicU64) }
    }

    fn jump_to_head(&mut self) {
        self.read_pos = self.atomic(OFF_WRITE_POS).load(Ordering::Acquire);
        self.read_seq = self.atomic(OFF_WRITE_SEQ).load(Ordering::Acquire);
    }

    /// Whether the producer restarted (new file or generation): reopen the path.
    pub fn stale(&self) -> bool {
        self.atomic(OFF_GENERATION).load(Ordering::Relaxed) != self.generation
    }

    pub fn poll(&mut self) -> Poll {
        if self.stale() {
            return Poll::Reset;
        }
        let write_pos = self.atomic(OFF_WRITE_POS).load(Ordering::Acquire);
        if self.read_pos >= write_pos {
            return Poll::Empty;
        }
        if write_pos - self.read_pos > self.data_region_size as u64 {
            self.resets += 1;
            self.jump_to_head();
            return Poll::Reset;
        }
        for _ in 0..2 {
            let offset = (self.read_pos as usize) % self.data_region_size;
            if offset + RECORD_HEADER_SIZE > self.data_region_size {
                self.read_pos += (self.data_region_size - offset) as u64;
                continue;
            }
            let base = HEADER_SIZE + offset;
            let seq = self.atomic(base).load(Ordering::Acquire);
            if seq != self.read_seq + 1 {
                // The writer skipped to the region start here.
                self.read_pos += (self.data_region_size - offset) as u64;
                continue;
            }
            // SAFETY: bounds checked against the region below before reading the payload.
            let (meta, data_len) = unsafe {
                let p = self.mmap_ptr.add(base);
                (
                    RecordMeta {
                        slot: ptr::read_unaligned(p.add(8) as *const u64),
                        parent_slot: ptr::read_unaligned(p.add(16) as *const u64),
                        batch_start: ptr::read_unaligned(p.add(24) as *const u32),
                        batch_end: ptr::read_unaligned(p.add(28) as *const u32),
                        entry_offset: ptr::read_unaligned(p.add(32) as *const u32),
                        entry_count: ptr::read_unaligned(p.add(36) as *const u32),
                        flags: ptr::read_unaligned(p.add(40) as *const u16),
                        t_publish_ns: ptr::read_unaligned(p.add(48) as *const u64),
                    },
                    ptr::read_unaligned(p.add(44) as *const u32) as usize,
                )
            };
            if offset + RECORD_HEADER_SIZE + data_len > self.data_region_size {
                self.resets += 1;
                self.jump_to_head();
                return Poll::Reset;
            }
            // SAFETY: in bounds (checked above).
            let payload = unsafe {
                std::slice::from_raw_parts(self.mmap_ptr.add(base + RECORD_HEADER_SIZE), data_len)
                    .to_vec()
            };
            fence(Ordering::Acquire);
            let seq_after = self.atomic(base).load(Ordering::Acquire);
            let write_pos_after = self.atomic(OFF_WRITE_POS).load(Ordering::Acquire);
            if seq_after != seq
                || write_pos_after.saturating_add(self.max_record)
                    > self.read_pos + self.data_region_size as u64
            {
                self.resets += 1;
                self.jump_to_head();
                return Poll::Reset;
            }
            self.read_pos += align8(RECORD_HEADER_SIZE + data_len) as u64;
            self.read_seq = seq;
            return Poll::Record(meta, payload);
        }
        Poll::Empty
    }
}

impl Drop for RingReader {
    fn drop(&mut self) {
        // SAFETY: mapping created in `open`.
        unsafe { libc::munmap(self.mmap_ptr as *mut libc::c_void, self.mmap_len) };
    }
}

#[cfg(test)]
pub mod test_writer {
    //! Minimal writer with the proxy's layout, for tests of the reader and the gate.
    use super::*;
    use std::io::Write;

    pub struct TestWriter {
        ptr: *mut u8,
        len: usize,
        region: usize,
        pos: u64,
        seq: u64,
    }

    impl TestWriter {
        pub fn create(path: &Path, region: usize) -> Self {
            let len = HEADER_SIZE + region;
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .unwrap();
            f.write_all(&vec![0u8; len]).unwrap();
            let ptr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    f.as_raw_fd(),
                    0,
                )
            } as *mut u8;
            unsafe {
                ptr::write_unaligned(ptr.add(OFF_DATA_REGION_SIZE) as *mut u64, region as u64);
                ptr::write_unaligned(ptr.add(OFF_MAX_RECORD) as *mut u64, (region / 4) as u64);
                ptr::write_unaligned(ptr.add(OFF_GENERATION) as *mut u64, 1);
                ptr::write_unaligned(ptr.add(OFF_MAGIC) as *mut u64, MAGIC_V2);
            }
            Self {
                ptr,
                len,
                region,
                pos: 0,
                seq: 0,
            }
        }

        pub fn publish(&mut self, m: &RecordMeta, payload: &[u8]) {
            let total = align8(RECORD_HEADER_SIZE + payload.len());
            let off = (self.pos as usize) % self.region;
            if off + total > self.region {
                self.pos += (self.region - off) as u64;
            }
            let off = (self.pos as usize) % self.region;
            self.seq += 1;
            unsafe {
                let p = self.ptr.add(HEADER_SIZE + off);
                (*(p as *const AtomicU64)).store(0, Ordering::Relaxed);
                ptr::write_unaligned(p.add(8) as *mut u64, m.slot);
                ptr::write_unaligned(p.add(16) as *mut u64, m.parent_slot);
                ptr::write_unaligned(p.add(24) as *mut u32, m.batch_start);
                ptr::write_unaligned(p.add(28) as *mut u32, m.batch_end);
                ptr::write_unaligned(p.add(32) as *mut u32, m.entry_offset);
                ptr::write_unaligned(p.add(36) as *mut u32, m.entry_count);
                ptr::write_unaligned(p.add(40) as *mut u16, m.flags);
                ptr::write_unaligned(p.add(44) as *mut u32, payload.len() as u32);
                ptr::write_unaligned(p.add(48) as *mut u64, m.t_publish_ns);
                ptr::copy_nonoverlapping(payload.as_ptr(), p.add(RECORD_HEADER_SIZE), payload.len());
                (*(p as *const AtomicU64)).store(self.seq, Ordering::Release);
                self.pos += total as u64;
                (*(self.ptr.add(OFF_WRITE_SEQ) as *const AtomicU64)).store(self.seq, Ordering::Release);
                (*(self.ptr.add(OFF_WRITE_POS) as *const AtomicU64)).store(self.pos, Ordering::Release);
            }
        }
    }

    impl Drop for TestWriter {
        fn drop(&mut self) {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, test_writer::TestWriter};

    #[test]
    fn test_read_in_order_across_wraps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ring");
        let mut w = TestWriter::create(&path, 1 << 16);
        let mut r = RingReader::open(&path).unwrap();
        assert!(matches!(r.poll(), Poll::Empty));
        let mut got = 0u32;
        for i in 0..3000u32 {
            let m = RecordMeta {
                slot: 9,
                parent_slot: 8,
                batch_start: i,
                batch_end: i + 1,
                entry_offset: i,
                entry_count: 1,
                flags: FLAG_FINAL,
                t_publish_ns: 5,
            };
            w.publish(&m, &vec![i as u8; (i as usize * 7) % 900]);
            loop {
                match r.poll() {
                    Poll::Record(meta, payload) => {
                        assert_eq!(meta.batch_start, got);
                        assert_eq!(payload.len(), (got as usize * 7) % 900);
                        assert!(meta.is_final() && !meta.last_in_slot());
                        got += 1;
                    }
                    Poll::Empty => break,
                    Poll::Reset => panic!("reset"),
                }
            }
        }
        assert_eq!(got, 3000);
    }

    #[test]
    fn test_lapped_reader_resets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ring");
        let mut w = TestWriter::create(&path, 1 << 14);
        let mut r = RingReader::open(&path).unwrap();
        for i in 0..500u32 {
            w.publish(
                &RecordMeta {
                    batch_start: i,
                    ..RecordMeta::default()
                },
                &[0u8; 400],
            );
        }
        assert!(matches!(r.poll(), Poll::Reset));
        assert_eq!(r.resets, 1);
    }
}
