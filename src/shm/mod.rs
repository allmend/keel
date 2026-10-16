//! One shared-memory region per master, written by the workers and read by
//! the control worker, the way nginx keeps its shared zones.
//!
//! The master maps it (`MAP_SHARED | MAP_ANONYMOUS`) before forking anything,
//! so every child inherits the same pages and the region lives exactly as
//! long as the master: no file, nothing to clean up, and a master restart
//! starts from zero. The master never reads it.
//!
//! ```text
//! header   globals: one u64 counter each, written by whoever owns them
//! index    open-addressing hash table of u32: 0 empty, BUSY being added,
//!          else an arena offset + 1
//! arena    one record per series: key length, first cell, cell count, key
//! stripes  one array of u64 cells per worker index, `cells` long each
//! ```
//!
//! A series is a key (opaque bytes) and a run of value cells. Any process
//! adds one by claiming an empty index slot (CAS to BUSY), writing the record
//! and publishing its offset, so creation is lock-free and the record is
//! complete before anyone can see it. Series are never removed. Every worker
//! writes only its own stripe, so workers never share a cache line, and a
//! replacement worker takes over its predecessor's stripe: totals carry on.

use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const HEADER: usize = 4096;
/// Header counters available to users of the region.
pub const GLOBALS: usize = 256;
const NEXT_CELL: usize = 0;
const NEXT_ARENA: usize = 1;
/// Offset of the globals in the header, in u64 words.
const GLOBALS_AT: usize = 8;

const BUSY: u32 = u32::MAX;
/// Record header: key length, first cell, cell count.
const RECORD: usize = 12;
/// A slot claimed by a process that died before publishing it would block
/// that key forever; after this long the claim is treated as someone else's.
const BUSY_WAIT: Duration = Duration::from_millis(5);

/// Sizes of a region, in elements.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub stripes: usize,
    /// Value cells per stripe.
    pub cells: usize,
    /// Index slots; a power of two.
    pub slots: usize,
    /// Arena bytes for the series records.
    pub arena: usize,
}

impl Layout {
    fn index_at(&self) -> usize {
        HEADER
    }
    fn arena_at(&self) -> usize {
        align(self.index_at() + self.slots * 4, 64)
    }
    fn stripes_at(&self) -> usize {
        align(self.arena_at() + self.arena, 64)
    }
    fn stripe_len(&self) -> usize {
        align(self.cells * 8, 64)
    }
    fn len(&self) -> usize {
        self.stripes_at() + self.stripes * self.stripe_len()
    }
}

fn align(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

/// A series as stored: its key and the first of its value cells.
pub struct Series<'a> {
    pub key: &'a [u8],
    pub first_cell: u32,
    pub cells: u32,
}

pub struct Region {
    base: NonNull<u8>,
    layout: Layout,
}

// SAFETY: the region is only ever accessed through atomics, except record
// bytes, which are written once before they are published with Release and
// never written again.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    /// Map a zeroed region. Pages never written use no memory.
    pub fn create(layout: Layout) -> io::Result<Region> {
        assert!(layout.slots.is_power_of_two() && layout.stripes > 0);
        assert!(layout.cells <= u32::MAX as usize && layout.arena < u32::MAX as usize);
        #[allow(unused_mut)]
        let mut flags = libc::MAP_SHARED | libc::MAP_ANON;
        #[cfg(target_os = "linux")]
        {
            flags |= libc::MAP_NORESERVE;
        }
        // SAFETY: a fresh anonymous mapping, not aliasing anything.
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), layout.len(), libc::PROT_READ | libc::PROT_WRITE, flags, -1, 0)
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = NonNull::new(ptr.cast()).ok_or_else(io::Error::last_os_error)?;
        Ok(Region { base, layout })
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    fn word(&self, i: usize) -> &AtomicU64 {
        // SAFETY: inside the header, 8-byte aligned (the mapping is page aligned).
        unsafe { &*self.base.as_ptr().add(i * 8).cast::<AtomicU64>() }
    }

    /// A counter in the header, shared by every process.
    pub fn global(&self, i: usize) -> &AtomicU64 {
        assert!(i < GLOBALS);
        self.word(GLOBALS_AT + i)
    }

    fn slot(&self, i: usize) -> &AtomicU32 {
        // SAFETY: i < slots, inside the index.
        unsafe { &*self.base.as_ptr().add(self.layout.index_at() + i * 4).cast::<AtomicU32>() }
    }

    /// Cell `cell` of stripe `stripe`.
    pub fn cell(&self, stripe: usize, cell: u32) -> &AtomicU64 {
        let l = &self.layout;
        assert!(stripe < l.stripes && (cell as usize) < l.cells);
        // SAFETY: bounds checked above; stripes are 64-byte aligned.
        unsafe { &*self.base.as_ptr().add(l.stripes_at() + stripe * l.stripe_len() + cell as usize * 8).cast::<AtomicU64>() }
    }

    fn record(&self, offset: u32) -> Series<'_> {
        // SAFETY: `offset` was published by `series` after the record was
        // written in full, and records are never written again.
        unsafe {
            let p = self.base.as_ptr().add(self.layout.arena_at() + offset as usize);
            let head = std::slice::from_raw_parts(p, RECORD);
            let n = |i: usize| u32::from_ne_bytes(head[i..i + 4].try_into().unwrap());
            Series { key: std::slice::from_raw_parts(p.add(RECORD), n(0) as usize), first_cell: n(4), cells: n(8) }
        }
    }

    /// The first cell of series `key`, adding the series with `cells` cells
    /// if it does not exist yet. `None` when the region is full.
    pub fn series(&self, key: &[u8], cells: u32) -> Option<u32> {
        let mask = self.layout.slots - 1;
        let mut i = fnv1a(key) as usize & mask;
        for _ in 0..self.layout.slots {
            let slot = self.slot(i);
            let mut v = slot.load(Ordering::Acquire);
            if v == 0 {
                match slot.compare_exchange(0, BUSY, Ordering::Acquire, Ordering::Acquire) {
                    Ok(_) => return self.add(slot, key, cells),
                    Err(now) => v = now,
                }
            }
            if v == BUSY {
                v = wait_published(slot);
            }
            if v != BUSY && v != 0 {
                let s = self.record(v - 1);
                if s.key == key {
                    return Some(s.first_cell);
                }
            }
            i = (i + 1) & mask;
        }
        None
    }

    /// Write the record for a slot this process claimed and publish it, or
    /// give the slot back when the region is full.
    fn add(&self, slot: &AtomicU32, key: &[u8], cells: u32) -> Option<u32> {
        let size = align(RECORD + key.len(), 4) as u64;
        let offset = self.word(NEXT_ARENA).fetch_add(size, Ordering::Relaxed);
        let first = self.word(NEXT_CELL).fetch_add(cells as u64, Ordering::Relaxed);
        if offset + size > self.layout.arena as u64 || first + cells as u64 > self.layout.cells as u64 {
            slot.store(0, Ordering::Release);
            return None;
        }
        // SAFETY: [offset, offset + size) was handed to this call alone.
        unsafe {
            let p = self.base.as_ptr().add(self.layout.arena_at() + offset as usize);
            let head = [(key.len() as u32).to_ne_bytes(), (first as u32).to_ne_bytes(), cells.to_ne_bytes()].concat();
            std::ptr::copy_nonoverlapping(head.as_ptr(), p, RECORD);
            std::ptr::copy_nonoverlapping(key.as_ptr(), p.add(RECORD), key.len());
        }
        slot.store(offset as u32 + 1, Ordering::Release);
        Some(first as u32)
    }

    /// Every published series. A key can show up twice only after a process
    /// died while adding it; readers merge by key.
    pub fn each_series(&self) -> impl Iterator<Item = Series<'_>> {
        (0..self.layout.slots).filter_map(move |i| match self.slot(i).load(Ordering::Acquire) {
            0 | BUSY => None,
            v => Some(self.record(v - 1)),
        })
    }
}

fn wait_published(slot: &AtomicU32) -> u32 {
    let start = Instant::now();
    loop {
        let v = slot.load(Ordering::Acquire);
        if v != BUSY || start.elapsed() > BUSY_WAIT {
            return v;
        }
        std::thread::yield_now();
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the mapping this struct created.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.layout.len()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> Region {
        Region::create(Layout { stripes: 2, cells: 64, slots: 16, arena: 1024 }).unwrap()
    }

    #[test]
    fn a_series_is_found_again_by_its_key() {
        let r = small();
        let a = r.series(b"a", 3).unwrap();
        let b = r.series(b"b", 1).unwrap();
        assert_eq!(r.series(b"a", 3), Some(a));
        assert_ne!(a, b);
        let mut keys: Vec<_> = r.each_series().map(|s| (s.key.to_vec(), s.cells)).collect();
        keys.sort();
        assert_eq!(keys, vec![(b"a".to_vec(), 3), (b"b".to_vec(), 1)]);
    }

    #[test]
    fn stripes_are_separate() {
        let r = small();
        let c = r.series(b"x", 1).unwrap();
        r.cell(0, c).fetch_add(2, Ordering::Relaxed);
        r.cell(1, c).fetch_add(5, Ordering::Relaxed);
        assert_eq!(r.cell(0, c).load(Ordering::Relaxed), 2);
        assert_eq!(r.cell(1, c).load(Ordering::Relaxed), 5);
    }

    #[test]
    fn a_full_region_refuses_new_series_and_keeps_the_old() {
        let r = small();
        let first = r.series(b"first", 60).unwrap();
        assert_eq!(r.series(b"second", 8), None);
        assert_eq!(r.series(b"first", 60), Some(first));
    }

    #[test]
    fn a_forked_child_writes_where_the_parent_reads() {
        let r = small();
        let c = r.series(b"shared", 1).unwrap();
        match unsafe { libc::fork() } {
            0 => {
                let added = r.series(b"from child", 1).unwrap();
                r.cell(1, c).fetch_add(7, Ordering::Relaxed);
                r.cell(1, added).fetch_add(1, Ordering::Relaxed);
                unsafe { libc::_exit(0) };
            }
            pid => {
                let mut status = 0;
                unsafe { libc::waitpid(pid, &mut status, 0) };
            }
        }
        assert_eq!(r.cell(1, c).load(Ordering::Relaxed), 7);
        let added = r.series(b"from child", 1).unwrap();
        assert_eq!(r.cell(1, added).load(Ordering::Relaxed), 1);
    }

    #[test]
    fn concurrent_adders_agree_on_one_series() {
        let r = std::sync::Arc::new(Region::create(Layout { stripes: 1, cells: 4096, slots: 1024, arena: 64 * 1024 }).unwrap());
        let firsts: Vec<Vec<u32>> = (0..8)
            .map(|_| {
                let r = r.clone();
                std::thread::spawn(move || (0..200).map(|k| r.series(format!("k{k}").as_bytes(), 2).unwrap()).collect())
            })
            .map(|t| t.join().unwrap())
            .collect();
        assert!(firsts.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(r.each_series().count(), 200);
    }
}
