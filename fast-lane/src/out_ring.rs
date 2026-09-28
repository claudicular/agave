//! Phase-3 output: the fast lane's account-update ring (`/dev/shm/fastlane.out.ring`).
//!
//! A single-producer shared-memory ring written by the coordinator thread when a transaction
//! becomes FINAL (so records are exact in FL's sense: validated, not yet compared with
//! agave), plus minimal slot markers. Consumers map the file read-only and spin-poll it.
//! Per-account block order holds because the coordinator emits FINAL events in per-account
//! block order; there is no cross-slot or cross-fork ordering beyond publication order.
//!
//! ## Layout (little-endian, 8-byte aligned)
//!
//! File header, [`HEADER_SIZE`] bytes:
//! ```text
//! 0x00 magic u64 = "FLOUTv01"      0x08 abi u32   0x0c header_size u32
//! 0x10 data_region_size u64        0x18 max_record u64
//! 0x20 generation u64 (producer start, CLOCK_REALTIME ns; a change = producer restart)
//! 0x28 producer_pid u32
//! 0x40 write_pos u64  (bytes published, monotonic; Release after each record)
//! 0x48 write_seq u64  (records published)
//! 0x80 heartbeat_ns u64 (CLOCK_REALTIME ns: each record, and ~1 ms when idle)
//! 0x88 records_dropped u64 (records too large for max_record)
//! ```
//! The data region follows. A record never wraps: when it does not fit before the region
//! end the producer writes `seq = 0` at the current offset (if 8 bytes fit) and restarts
//! at offset 0; a reader that finds a sequence other than `expected + 1` (or fewer than
//! [`RECORD_HEADER_SIZE`] bytes left) skips to the region start.
//!
//! Record header, [`RECORD_HEADER_SIZE`] bytes:
//! ```text
//! 0  seq u64      (0 while being written; the record's sequence number, stored last)
//! 8  kind u16     10 flags u16     12 len u32 (whole record, 8-aligned)
//! 16 slot u64     24 parent_slot u64
//! 32 tx_ordinal u32 (index of the transaction in the slot = agave's transaction index;
//!                    SLOT_END: number of transactions)
//! 36 n_accounts u16   38 incarnations u16
//! 40 fork_id u64  (FL run id: one per (slot, parent bank) FL executed)
//! 48 t_publish_ns u64 (CLOCK_REALTIME when the record was written)
//! 56 t_source_ns u64  (TX: when FL received the transaction's data — the proxy ring's
//!                      publish time, or agave's data-set completion)
//! ```
//! `TX` body: `signature [64] | err u32 (0 = success) | cu u32`, then `n_accounts` accounts:
//! `pubkey [32] | owner [32] | lamports u64 | data_len u32 | flags u8 | pad [3] | data`,
//! each padded to 8 bytes. Accounts are the transaction's grouped-notification accounts
//! (agave's `notify_transaction_accounts` set: written accounts plus read-only accounts of
//! the configured read-only owners) whose **owner** is in the ring's owner filter; a
//! transaction with none is not published. This is the matching rule of Yellowstone's
//! `transaction_accounts` owner filter with `include_all_accounts = false`.
//! `ROLLBACK` body: a 32-byte, zero-padded reason. `SLOT_BEGIN` / `SLOT_END`: no body.

use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_pubkey::Pubkey,
    std::{
        fs::OpenOptions,
        io,
        os::unix::io::AsRawFd,
        path::Path,
        ptr,
        sync::atomic::{AtomicU64, Ordering, fence},
    },
};

pub const MAGIC: u64 = u64::from_le_bytes(*b"FLOUTv01");
pub const ABI_VERSION: u32 = 1;
pub const HEADER_SIZE: usize = 256;
pub const RECORD_HEADER_SIZE: usize = 64;
pub const TX_BODY_FIXED: usize = 72;
pub const ACCOUNT_HEADER_SIZE: usize = 80;

pub const KIND_TX: u16 = 1;
pub const KIND_SLOT_BEGIN: u16 = 2;
pub const KIND_SLOT_END: u16 = 3;
pub const KIND_ROLLBACK: u16 = 4;

/// TX: the transaction succeeded.
pub const FLAG_OK: u16 = 1;
pub const FLAG_VOTE: u16 = 2;
/// The slot ran on FL's own run of an unfrozen parent (phase 2b).
pub const FLAG_CHAINED: u16 = 4;
/// The transaction came from the proxy ring (else from agave's blockstore).
pub const FLAG_FROM_RING: u16 = 8;
/// The validated incarnation ran while some predecessor was not FINAL.
pub const FLAG_SPECULATIVE: u16 = 16;
/// Some matching accounts were left out (the record would exceed `max_record`).
pub const FLAG_INCOMPLETE: u16 = 32;

pub const ACCT_WRITTEN: u8 = 1;
pub const ACCT_EXECUTABLE: u8 = 2;

const OFF_MAGIC: usize = 0x00;
const OFF_ABI: usize = 0x08;
const OFF_HEADER_SIZE: usize = 0x0c;
const OFF_DATA_REGION_SIZE: usize = 0x10;
const OFF_MAX_RECORD: usize = 0x18;
const OFF_GENERATION: usize = 0x20;
const OFF_PID: usize = 0x28;
const OFF_WRITE_POS: usize = 0x40;
const OFF_WRITE_SEQ: usize = 0x48;
const OFF_HEARTBEAT: usize = 0x80;
const OFF_DROPPED: usize = 0x88;

#[inline]
pub const fn align8(n: usize) -> usize {
    (n + 7) & !7
}

pub fn unix_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Fixed header fields of a record (everything but `seq` and `len`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordHeader {
    pub kind: u16,
    pub flags: u16,
    pub slot: u64,
    pub parent_slot: u64,
    pub tx_ordinal: u32,
    pub n_accounts: u16,
    pub incarnations: u16,
    pub fork_id: u64,
    pub t_publish_ns: u64,
    pub t_source_ns: u64,
}

/// One account of a TX record.
#[derive(Debug, Clone, Copy)]
pub struct AccountRef<'a> {
    pub pubkey: &'a Pubkey,
    pub account: &'a AccountSharedData,
    pub written: bool,
}

impl AccountRef<'_> {
    pub fn encoded_len(&self) -> usize {
        ACCOUNT_HEADER_SIZE + align8(self.account.data().len())
    }
}

/// The producer side. Only one thread may write.
pub struct OutRing {
    ptr: *mut u8,
    len: usize,
    region: usize,
    max_record: usize,
    pos: u64,
    seq: u64,
}

// SAFETY: one writer thread owns it; the mapping lives until Drop.
unsafe impl Send for OutRing {}

impl OutRing {
    /// Create (or take over) the ring file at `path` with a `region`-byte data region.
    /// An existing file of the same size is reused (readers holding it see a new
    /// generation); otherwise it is replaced.
    pub fn create(path: &Path, region: usize) -> io::Result<Self> {
        if region < 1 << 16 || region % 8 != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "region too small"));
        }
        let len = HEADER_SIZE + region;
        let reuse = std::fs::metadata(path)
            .map(|m| m.len() == len as u64)
            .unwrap_or(false);
        if !reuse {
            let _ = std::fs::remove_file(path);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.set_len(len as u64)?;
        #[cfg(target_os = "linux")]
        let flags = libc::MAP_SHARED | libc::MAP_POPULATE;
        #[cfg(not(target_os = "linux"))]
        let flags = libc::MAP_SHARED;
        // SAFETY: shared read-write mapping of a regular file of `len` bytes.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = ptr as *mut u8;
        // Best effort: keep the ring resident (fails without CAP_IPC_LOCK / memlock limit).
        // SAFETY: the mapping is `len` bytes.
        unsafe {
            let _ = libc::mlock(ptr as *const libc::c_void, len);
        }
        let ring = Self {
            ptr,
            len,
            region,
            max_record: region / 8,
            pos: 0,
            seq: 0,
        };
        // Invalidate first (readers of a reused file see a bad magic, then the new
        // generation), then publish the header.
        ring.atomic(OFF_MAGIC).store(0, Ordering::SeqCst);
        ring.atomic(OFF_WRITE_POS).store(0, Ordering::SeqCst);
        ring.atomic(OFF_WRITE_SEQ).store(0, Ordering::SeqCst);
        // SAFETY: header offsets are inside the mapping.
        unsafe {
            ptr::write_unaligned(ptr.add(OFF_ABI) as *mut u32, ABI_VERSION);
            ptr::write_unaligned(ptr.add(OFF_HEADER_SIZE) as *mut u32, HEADER_SIZE as u32);
            ptr::write_unaligned(ptr.add(OFF_DATA_REGION_SIZE) as *mut u64, region as u64);
            ptr::write_unaligned(ptr.add(OFF_MAX_RECORD) as *mut u64, ring.max_record as u64);
            ptr::write_unaligned(ptr.add(OFF_PID) as *mut u32, std::process::id());
            // Clear the region's first record header so no stale sequence survives.
            ptr::write_bytes(ptr.add(HEADER_SIZE), 0, RECORD_HEADER_SIZE);
        }
        ring.atomic(OFF_DROPPED).store(0, Ordering::SeqCst);
        ring.atomic(OFF_GENERATION).store(unix_ns().max(1), Ordering::SeqCst);
        ring.atomic(OFF_HEARTBEAT).store(unix_ns(), Ordering::SeqCst);
        ring.atomic(OFF_MAGIC).store(MAGIC, Ordering::SeqCst);
        Ok(ring)
    }

    fn atomic(&self, off: usize) -> &AtomicU64 {
        // SAFETY: 8-aligned offsets inside the mapping.
        unsafe { &*(self.ptr.add(off) as *const AtomicU64) }
    }

    pub fn max_record(&self) -> usize {
        self.max_record
    }

    pub fn heartbeat(&self) {
        self.atomic(OFF_HEARTBEAT).store(unix_ns(), Ordering::Release);
    }

    pub fn count_dropped(&self) {
        self.atomic(OFF_DROPPED).fetch_add(1, Ordering::Relaxed);
    }

    /// Write one record of `len` bytes (8-aligned, header included): `fill` writes the
    /// body at `p + RECORD_HEADER_SIZE`. Returns false if the record is too large.
    fn publish_raw(&mut self, header: &RecordHeader, len: usize, fill: impl FnOnce(*mut u8)) -> bool {
        debug_assert_eq!(len % 8, 0);
        if len > self.max_record {
            self.count_dropped();
            return false;
        }
        let mut off = (self.pos % self.region as u64) as usize;
        if off + len > self.region {
            if off + 8 <= self.region {
                // SAFETY: in bounds; marks "skip to the region start" for readers.
                unsafe {
                    (*(self.ptr.add(HEADER_SIZE + off) as *const AtomicU64)).store(0, Ordering::Relaxed)
                };
            }
            self.pos += (self.region - off) as u64;
            off = 0;
        }
        self.seq += 1;
        // SAFETY: [off, off + len) is inside the data region.
        unsafe {
            let p = self.ptr.add(HEADER_SIZE + off);
            let seq_word = &*(p as *const AtomicU64);
            seq_word.store(0, Ordering::Relaxed);
            fence(Ordering::Release);
            ptr::write_unaligned(p.add(8) as *mut u16, header.kind);
            ptr::write_unaligned(p.add(10) as *mut u16, header.flags);
            ptr::write_unaligned(p.add(12) as *mut u32, len as u32);
            ptr::write_unaligned(p.add(16) as *mut u64, header.slot);
            ptr::write_unaligned(p.add(24) as *mut u64, header.parent_slot);
            ptr::write_unaligned(p.add(32) as *mut u32, header.tx_ordinal);
            ptr::write_unaligned(p.add(36) as *mut u16, header.n_accounts);
            ptr::write_unaligned(p.add(38) as *mut u16, header.incarnations);
            ptr::write_unaligned(p.add(40) as *mut u64, header.fork_id);
            ptr::write_unaligned(p.add(48) as *mut u64, header.t_publish_ns);
            ptr::write_unaligned(p.add(56) as *mut u64, header.t_source_ns);
            fill(p.add(RECORD_HEADER_SIZE));
            seq_word.store(self.seq, Ordering::Release);
        }
        self.pos += len as u64;
        self.atomic(OFF_WRITE_SEQ).store(self.seq, Ordering::Release);
        self.atomic(OFF_WRITE_POS).store(self.pos, Ordering::Release);
        self.atomic(OFF_HEARTBEAT).store(header.t_publish_ns, Ordering::Relaxed);
        true
    }

    /// A marker record (SLOT_BEGIN / SLOT_END / ROLLBACK).
    pub fn publish_marker(&mut self, mut header: RecordHeader, reason: Option<&str>) -> bool {
        header.n_accounts = 0;
        header.t_publish_ns = unix_ns();
        let body = if reason.is_some() { 32 } else { 0 };
        self.publish_raw(&header, RECORD_HEADER_SIZE + body, |p| {
            if let Some(reason) = reason {
                let bytes = reason.as_bytes();
                let n = bytes.len().min(32);
                // SAFETY: 32 body bytes reserved.
                unsafe {
                    ptr::write_bytes(p, 0, 32);
                    ptr::copy_nonoverlapping(bytes.as_ptr(), p, n);
                }
            }
        })
    }

    /// A TX record. Accounts that would push the record past `max_record` are left out
    /// (`FLAG_INCOMPLETE`). `header.t_publish_ns` is set here. Returns `None` if nothing
    /// was published, else whether accounts were left out.
    pub fn publish_tx(
        &mut self,
        mut header: RecordHeader,
        signature: &[u8; 64],
        err: u32,
        cu: u32,
        accounts: &[AccountRef<'_>],
    ) -> Option<bool> {
        let mut len = RECORD_HEADER_SIZE + TX_BODY_FIXED;
        let mut n = 0usize;
        for account in accounts {
            let add = account.encoded_len();
            if len + add > self.max_record {
                header.flags |= FLAG_INCOMPLETE;
                continue;
            }
            len += add;
            n += 1;
        }
        header.kind = KIND_TX;
        header.n_accounts = n as u16;
        header.t_publish_ns = unix_ns();
        let incomplete = header.flags & FLAG_INCOMPLETE != 0;
        let max_record = self.max_record;
        let published = self.publish_raw(&header, len, |p| {
            // SAFETY: `len` bytes were reserved; each account below was counted in it.
            unsafe {
                ptr::copy_nonoverlapping(signature.as_ptr(), p, 64);
                ptr::write_unaligned(p.add(64) as *mut u32, err);
                ptr::write_unaligned(p.add(68) as *mut u32, cu);
                let mut q = p.add(TX_BODY_FIXED);
                let mut used = RECORD_HEADER_SIZE + TX_BODY_FIXED;
                for account in accounts {
                    let add = account.encoded_len();
                    if used + add > max_record {
                        continue;
                    }
                    used += add;
                    let data = account.account.data();
                    ptr::copy_nonoverlapping(account.pubkey.as_ref().as_ptr(), q, 32);
                    ptr::copy_nonoverlapping(account.account.owner().as_ref().as_ptr(), q.add(32), 32);
                    ptr::write_unaligned(q.add(64) as *mut u64, account.account.lamports());
                    ptr::write_unaligned(q.add(72) as *mut u32, data.len() as u32);
                    let mut flags = 0u8;
                    if account.written {
                        flags |= ACCT_WRITTEN;
                    }
                    if account.account.executable() {
                        flags |= ACCT_EXECUTABLE;
                    }
                    *q.add(76) = flags;
                    *q.add(77) = 0;
                    *q.add(78) = 0;
                    *q.add(79) = 0;
                    ptr::copy_nonoverlapping(data.as_ptr(), q.add(ACCOUNT_HEADER_SIZE), data.len());
                    q = q.add(add);
                }
            }
        });
        published.then_some(incomplete)
    }
}

impl Drop for OutRing {
    fn drop(&mut self) {
        // SAFETY: mapping created in `create`.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

/// A decoded account of a TX record (owned copy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountView {
    pub pubkey: Pubkey,
    pub owner: Pubkey,
    pub lamports: u64,
    pub flags: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub seq: u64,
    pub header: RecordHeader,
    /// TX only.
    pub signature: [u8; 64],
    pub err: u32,
    pub cu: u32,
    pub accounts: Vec<AccountView>,
    /// ROLLBACK only.
    pub reason: String,
}

pub enum Poll {
    Record(Box<Record>),
    Empty,
    /// Lapped, a torn read, or the producer restarted: positions were lost.
    Reset,
}

/// Reference consumer (tests, tools). geyserbench's `fastlane_ring` provider mirrors it.
pub struct OutRingReader {
    ptr: *const u8,
    len: usize,
    region: usize,
    max_record: u64,
    generation: u64,
    read_pos: u64,
    read_seq: u64,
    pub resets: u64,
}

// SAFETY: one reader thread owns it.
unsafe impl Send for OutRingReader {}

impl OutRingReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let len = file.metadata()?.len() as usize;
        if len < HEADER_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "ring too small"));
        }
        // SAFETY: read-only shared mapping; unmapped in Drop.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = ptr as *const u8;
        let mut reader = Self {
            ptr,
            len,
            region: 0,
            max_record: 0,
            generation: 0,
            read_pos: 0,
            read_seq: 0,
            resets: 0,
        };
        if reader.atomic(OFF_MAGIC).load(Ordering::Acquire) != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
        }
        // SAFETY: header fields inside the mapping.
        let (abi, region, max_record) = unsafe {
            (
                ptr::read_unaligned(ptr.add(OFF_ABI) as *const u32),
                ptr::read_unaligned(ptr.add(OFF_DATA_REGION_SIZE) as *const u64) as usize,
                ptr::read_unaligned(ptr.add(OFF_MAX_RECORD) as *const u64),
            )
        };
        if abi != ABI_VERSION || region == 0 || HEADER_SIZE + region > len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad ring header"));
        }
        reader.region = region;
        reader.max_record = max_record;
        reader.generation = reader.atomic(OFF_GENERATION).load(Ordering::Acquire);
        reader.jump_to_head();
        Ok(reader)
    }

    fn atomic(&self, off: usize) -> &AtomicU64 {
        // SAFETY: 8-aligned offsets inside the mapping.
        unsafe { &*(self.ptr.add(off) as *const AtomicU64) }
    }

    fn jump_to_head(&mut self) {
        self.read_seq = self.atomic(OFF_WRITE_SEQ).load(Ordering::Acquire);
        self.read_pos = self.atomic(OFF_WRITE_POS).load(Ordering::Acquire);
    }

    /// The producer restarted (reopen the path).
    pub fn stale(&self) -> bool {
        self.atomic(OFF_GENERATION).load(Ordering::Relaxed) != self.generation
            || self.atomic(OFF_MAGIC).load(Ordering::Relaxed) != MAGIC
    }

    pub fn heartbeat_ns(&self) -> u64 {
        self.atomic(OFF_HEARTBEAT).load(Ordering::Relaxed)
    }

    pub fn poll(&mut self) -> Poll {
        if self.stale() {
            return Poll::Reset;
        }
        let write_pos = self.atomic(OFF_WRITE_POS).load(Ordering::Acquire);
        if self.read_pos >= write_pos {
            return Poll::Empty;
        }
        if write_pos - self.read_pos > self.region as u64 {
            return self.reset();
        }
        for _ in 0..2 {
            let off = (self.read_pos % self.region as u64) as usize;
            if off + RECORD_HEADER_SIZE > self.region {
                self.read_pos += (self.region - off) as u64;
                continue;
            }
            let base = HEADER_SIZE + off;
            let seq = self.atomic(base).load(Ordering::Acquire);
            if seq != self.read_seq + 1 {
                self.read_pos += (self.region - off) as u64;
                continue;
            }
            // SAFETY: header inside the region; the body is bounds-checked against `len`.
            let record = unsafe { self.parse(base, off) };
            fence(Ordering::Acquire);
            let seq_after = self.atomic(base).load(Ordering::Acquire);
            let write_pos_after = self.atomic(OFF_WRITE_POS).load(Ordering::Acquire);
            let Some((record, len)) = record else {
                return self.reset();
            };
            if seq_after != seq
                || write_pos_after.saturating_add(self.max_record)
                    > self.read_pos + self.region as u64
            {
                return self.reset();
            }
            self.read_pos += len as u64;
            self.read_seq = seq;
            let mut record = record;
            record.seq = seq;
            return Poll::Record(Box::new(record));
        }
        Poll::Empty
    }

    fn reset(&mut self) -> Poll {
        self.resets += 1;
        self.jump_to_head();
        Poll::Reset
    }

    /// # Safety
    /// `base` is the offset of a record header inside the data region at region offset `off`.
    unsafe fn parse(&self, base: usize, off: usize) -> Option<(Record, usize)> {
        let p = unsafe { self.ptr.add(base) };
        let rd64 = |o: usize| unsafe { ptr::read_unaligned(p.add(o) as *const u64) };
        let rd32 = |o: usize| unsafe { ptr::read_unaligned(p.add(o) as *const u32) };
        let rd16 = |o: usize| unsafe { ptr::read_unaligned(p.add(o) as *const u16) };
        let len = rd32(12) as usize;
        if len < RECORD_HEADER_SIZE || len % 8 != 0 || off + len > self.region {
            return None;
        }
        let header = RecordHeader {
            kind: rd16(8),
            flags: rd16(10),
            slot: rd64(16),
            parent_slot: rd64(24),
            tx_ordinal: rd32(32),
            n_accounts: rd16(36),
            incarnations: rd16(38),
            fork_id: rd64(40),
            t_publish_ns: rd64(48),
            t_source_ns: rd64(56),
        };
        let mut record = Record {
            seq: 0,
            header,
            signature: [0; 64],
            err: 0,
            cu: 0,
            accounts: Vec::new(),
            reason: String::new(),
        };
        let body = unsafe { std::slice::from_raw_parts(p.add(RECORD_HEADER_SIZE), len - RECORD_HEADER_SIZE) };
        match header.kind {
            KIND_TX => {
                if body.len() < TX_BODY_FIXED {
                    return None;
                }
                record.signature.copy_from_slice(&body[..64]);
                record.err = u32::from_le_bytes(body[64..68].try_into().ok()?);
                record.cu = u32::from_le_bytes(body[68..72].try_into().ok()?);
                let mut o = TX_BODY_FIXED;
                for _ in 0..header.n_accounts {
                    if o + ACCOUNT_HEADER_SIZE > body.len() {
                        return None;
                    }
                    let a = &body[o..];
                    let data_len = u32::from_le_bytes(a[72..76].try_into().ok()?) as usize;
                    let total = ACCOUNT_HEADER_SIZE + align8(data_len);
                    if o + total > body.len() {
                        return None;
                    }
                    record.accounts.push(AccountView {
                        pubkey: Pubkey::new_from_array(a[..32].try_into().ok()?),
                        owner: Pubkey::new_from_array(a[32..64].try_into().ok()?),
                        lamports: u64::from_le_bytes(a[64..72].try_into().ok()?),
                        flags: a[76],
                        data: a[ACCOUNT_HEADER_SIZE..ACCOUNT_HEADER_SIZE + data_len].to_vec(),
                    });
                    o += total;
                }
            }
            KIND_ROLLBACK => {
                let raw = &body[..body.len().min(32)];
                let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
                record.reason = String::from_utf8_lossy(&raw[..end]).into_owned();
            }
            _ => {}
        }
        Some((record, len))
    }
}

impl Drop for OutRingReader {
    fn drop(&mut self) {
        // SAFETY: mapping created in `open`.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(lamports: u64, len: usize, owner: Pubkey, fill: u8) -> AccountSharedData {
        let mut a = AccountSharedData::new(lamports, len, &owner);
        solana_account::WritableAccount::data_as_mut_slice(&mut a).fill(fill);
        a
    }

    #[test]
    fn test_tx_records_roundtrip_across_wraps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.ring");
        let mut w = OutRing::create(&path, 1 << 16).unwrap();
        let mut r = OutRingReader::open(&path).unwrap();
        assert!(matches!(r.poll(), Poll::Empty));
        let owner = Pubkey::new_unique();
        let keys: Vec<Pubkey> = (0..4).map(|_| Pubkey::new_unique()).collect();
        let mut got = 0u32;
        for i in 0..2000u32 {
            let accounts: Vec<AccountSharedData> = (0..(i % 4) as usize)
                .map(|j| account(1000 + u64::from(i), (i as usize * 13 + j) % 300, owner, i as u8))
                .collect();
            let refs: Vec<AccountRef> = accounts
                .iter()
                .enumerate()
                .map(|(j, a)| AccountRef {
                    pubkey: &keys[j],
                    account: a,
                    written: j % 2 == 0,
                })
                .collect();
            let mut sig = [0u8; 64];
            sig[..4].copy_from_slice(&i.to_le_bytes());
            let header = RecordHeader {
                flags: FLAG_OK | FLAG_FROM_RING,
                slot: 100 + u64::from(i / 50),
                parent_slot: 99,
                tx_ordinal: i,
                incarnations: 1,
                fork_id: 7,
                t_source_ns: 5,
                ..RecordHeader::default()
            };
            assert_eq!(w.publish_tx(header, &sig, 0, 1234, &refs), Some(false));
            if i % 10 == 9 {
                assert!(w.publish_marker(
                    RecordHeader {
                        kind: KIND_SLOT_END,
                        slot: 100,
                        tx_ordinal: i,
                        ..RecordHeader::default()
                    },
                    None
                ));
            }
            loop {
                match r.poll() {
                    Poll::Record(rec) => {
                        if rec.header.kind == KIND_SLOT_END {
                            assert_eq!(rec.header.tx_ordinal, got - 1);
                            continue;
                        }
                        assert_eq!(rec.header.kind, KIND_TX);
                        assert_eq!(rec.header.tx_ordinal, got);
                        assert_eq!(&rec.signature[..4], &got.to_le_bytes());
                        assert_eq!(rec.cu, 1234);
                        assert_eq!(rec.header.flags, FLAG_OK | FLAG_FROM_RING);
                        assert_eq!(rec.accounts.len(), (got % 4) as usize);
                        for (j, a) in rec.accounts.iter().enumerate() {
                            assert_eq!(a.pubkey, keys[j]);
                            assert_eq!(a.owner, owner);
                            assert_eq!(a.lamports, 1000 + u64::from(got));
                            assert_eq!(a.data.len(), (got as usize * 13 + j) % 300);
                            assert!(a.data.iter().all(|b| *b == got as u8));
                            assert_eq!(a.flags & ACCT_WRITTEN != 0, j % 2 == 0);
                        }
                        assert!(rec.header.t_publish_ns > 0);
                        got += 1;
                    }
                    Poll::Empty => break,
                    Poll::Reset => panic!("reset at {got}"),
                }
            }
        }
        assert_eq!(got, 2000);
        assert_eq!(r.resets, 0);
    }

    /// The byte layout of one TX record, pinned (geyserbench's `fastlane_ring` reader tests
    /// parse these exact bytes). `t_publish_ns` (offset 48) is zeroed before comparing.
    pub const GOLDEN_TX: &str = concat!(
        "010000000000000001000900e000000039300000000000003830000000000000",
        "0500000001000200070000000000000000000000000000002a00000000000000",
        "0101010101010101010101010101010101010101010101010101010101010101",
        "0101010101010101010101010101010101010101010101010101010101010101",
        "00000000d2040000020202020202020202020202020202020202020202020202",
        "020202020202020206ddf6e1d765a193d9cbe146ceeb79ac1cb485ed5f5b3791",
        "3a8cf5857eff00a980000000000000000300000001000000aabbcc0000000000",
    );

    #[test]
    fn test_abi_golden() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.ring");
        let mut w = OutRing::create(&path, 1 << 16).unwrap();
        let key = Pubkey::new_from_array([2; 32]);
        let token: Pubkey = crate::output::TOKEN_PROGRAM;
        let mut a = AccountSharedData::new(128, 3, &token);
        solana_account::WritableAccount::data_as_mut_slice(&mut a).copy_from_slice(&[0xaa, 0xbb, 0xcc]);
        let header = RecordHeader {
            flags: FLAG_OK | FLAG_FROM_RING,
            slot: 12345,
            parent_slot: 12344,
            tx_ordinal: 5,
            incarnations: 2,
            fork_id: 7,
            t_source_ns: 42,
            ..RecordHeader::default()
        };
        w.publish_tx(
            header,
            &[1; 64],
            0,
            1234,
            &[AccountRef {
                pubkey: &key,
                account: &a,
                written: true,
            }],
        )
        .unwrap();
        let len = RECORD_HEADER_SIZE + TX_BODY_FIXED + ACCOUNT_HEADER_SIZE + 8;
        let mut bytes = vec![0u8; len];
        unsafe { ptr::copy_nonoverlapping(w.ptr.add(HEADER_SIZE), bytes.as_mut_ptr(), len) };
        bytes[48..56].fill(0);
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("GOLDEN {hex}");
        assert_eq!(hex, GOLDEN_TX);
    }

    #[test]
    fn test_lap_generation_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.ring");
        let mut w = OutRing::create(&path, 1 << 16).unwrap();
        let mut r = OutRingReader::open(&path).unwrap();
        let owner = Pubkey::new_unique();
        let key = Pubkey::new_unique();
        let a = account(1, 400, owner, 1);
        for _ in 0..500 {
            w.publish_tx(
                RecordHeader::default(),
                &[1; 64],
                0,
                0,
                &[AccountRef {
                    pubkey: &key,
                    account: &a,
                    written: true,
                }],
            );
        }
        assert!(matches!(r.poll(), Poll::Reset), "lapped reader resets");
        assert!(matches!(r.poll(), Poll::Empty), "then continues from the head");
        // A too-large account is left out and flagged.
        let big = account(1, (1 << 16) / 8, owner, 2);
        assert_eq!(w.publish_tx(
            RecordHeader::default(),
            &[2; 64],
            0,
            0,
            &[
                AccountRef {
                    pubkey: &key,
                    account: &a,
                    written: true,
                },
                AccountRef {
                    pubkey: &key,
                    account: &big,
                    written: true,
                },
            ],
        ), Some(true));
        match r.poll() {
            Poll::Record(rec) => {
                assert_eq!(rec.accounts.len(), 1);
                assert!(rec.header.flags & FLAG_INCOMPLETE != 0);
            }
            _ => panic!("record expected"),
        }
        // Rollback marker with a reason.
        w.publish_marker(
            RecordHeader {
                kind: KIND_ROLLBACK,
                slot: 9,
                fork_id: 3,
                ..RecordHeader::default()
            },
            Some("dead"),
        );
        match r.poll() {
            Poll::Record(rec) => {
                assert_eq!(rec.header.kind, KIND_ROLLBACK);
                assert_eq!(rec.reason, "dead");
                assert_eq!(rec.header.fork_id, 3);
            }
            _ => panic!("record expected"),
        }
        // A producer restart on the same file: the old reader sees a new generation.
        drop(w);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let _w2 = OutRing::create(&path, 1 << 16).unwrap();
        assert!(r.stale());
        assert!(matches!(r.poll(), Poll::Reset));
        let mut r2 = OutRingReader::open(&path).unwrap();
        assert!(matches!(r2.poll(), Poll::Empty));
    }
}
