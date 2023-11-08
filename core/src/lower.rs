//! Lower allocator implementations

use core::fmt::Write;
use core::mem::size_of;
use core::ops::Range;

use alloc::boxed::Box;
use alloc::slice;
use alloc::string::String;

use log::{error, info, warn};

use crate::atomic::Atom;
use crate::entry::{AtomicArray, HugeEntry, HugePair};
use crate::frame::{Frame, PFN};
use crate::util::{align_down, align_up, spin_wait, Align};
use crate::{Error, Init, Result, CAS_RETRIES};

type Bitfield = crate::bitfield::Bitfield<8>;

/// Lower-level frame allocator.
///
/// This level implements the actual allocation/free operations.
/// Each allocation/free is limited to a chunk of [LowerAlloc::N] frames.
///
/// Here the bitfields are 512 bit large -> strong focus on huge frames.
/// Upon that is a table for each tree, with an entry per bitfield.
///
/// The parameter `HP` configures the number of table entries (huge frames per tree).
/// It has to be a multiple of 2!
///
/// ## Memory Layout
/// **persistent:**
/// ```text
/// NVRAM: [ Frames | Bitfields | Tables | Zone ]
/// ```
/// **volatile:**
/// ```text
/// RAM: [ Frames ], Bitfields and Tables are allocated elswhere
/// ```
#[derive(Default, Debug)]
pub struct Lower {
    begin: PFN,
    len: usize,
    bitfields: Box<[Align<Bitfield>]>,
    children: Box<[Align<[Atom<HugeEntry>; Self::HP]>]>,
    persistent: bool,
}

unsafe impl Send for Lower {}
unsafe impl Sync for Lower {}

const _: () = assert!(Lower::HP < (1 << (u16::BITS as usize - Lower::HUGE_ORDER)));

impl Lower {
    /// Number of huge pages managed by a chunk
    pub const HP: usize = 32;
    /// Pages per chunk. Every alloc only searches in a chunk of this size.
    pub const N: usize = Self::HP * Bitfield::LEN;
    /// The maximal allowed order of this allocator
    pub const HUGE_ORDER: usize = Bitfield::ORDER;
    pub const MAX_ORDER: usize = Self::HUGE_ORDER + 1;

    /// Create a new lower allocator.
    pub fn new(_cores: usize, begin: PFN, len: usize, init: Init, free_all: bool) -> Self {
        debug_assert!(Self::HP < (1 << (u16::BITS as usize - Self::HUGE_ORDER)));

        // FIXME: Lifetime hack!
        let memory = unsafe { slice::from_raw_parts_mut(begin.as_ptr_mut(), len) };

        let num_bitfields = len.div_ceil(Bitfield::LEN);
        let num_tables = len.div_ceil(Self::N);
        let alloc = if init != Init::Volatile {
            // Reserve memory within the managed NVM for the l1 and l2 tables
            // These tables are stored at the end of the NVM
            let size_bitfields = num_bitfields * size_of::<Bitfield>();
            let size_bitfields = align_up(size_bitfields, size_of::<[HugeEntry; Self::HP]>()); // correct alignment
            let size_tables = num_bitfields * size_of::<[HugeEntry; Self::HP]>();
            // Num of frames occupied by the tables
            let metadata_frames = (size_bitfields + size_tables).div_ceil(Frame::SIZE);

            debug_assert!(metadata_frames < len);
            let len = len - metadata_frames;
            let (_, metadata) = memory.split_at_mut(len);

            let metadata: *mut u8 = metadata.as_mut_ptr().cast();

            // Start of the l1 table array
            let bitfields =
                unsafe { Box::from_raw(slice::from_raw_parts_mut(metadata.cast(), num_bitfields)) };

            let mut offset = num_bitfields * size_of::<Bitfield>();
            offset = align_up(offset, size_of::<[HugeEntry; Self::HP]>()); // correct alignment

            // Start of the l2 table array
            let children = unsafe {
                Box::from_raw(slice::from_raw_parts_mut(
                    metadata.add(offset).cast(),
                    num_tables,
                ))
            };

            Self {
                begin,
                len,
                bitfields,
                children,
                persistent: init != Init::Volatile,
            }
        } else {
            // Allocate l1 and l2 tables in volatile memory
            Self {
                begin,
                len,
                bitfields: unsafe { Box::new_uninit_slice(num_bitfields).assume_init() },
                children: unsafe { Box::new_uninit_slice(num_tables).assume_init() },
                persistent: init != Init::Volatile,
            }
        };
        // Skip for manual recovery using `Self::recover`
        if init != Init::Recover {
            if free_all {
                alloc.free_all();
            } else {
                alloc.reserve_all();
            }
        }
        alloc
    }

    pub fn frames(&self) -> usize {
        self.len
    }

    pub fn begin(&self) -> PFN {
        self.begin
    }

    pub fn memory(&self) -> Range<PFN> {
        self.begin()..PFN(self.begin().0 + self.frames())
    }

    /// Recovers the data structures for the [LowerAlloc::N] sized chunk at `start`.
    /// `deep` indicates that the allocator has crashed and the
    /// recovery might have to be more extensive.
    /// Returns the number of recovered frames.
    pub fn recover(&self, start: usize, deep: bool) -> Result<usize> {
        let mut frames = 0;

        let table = &self.children[start / Self::N];
        for (i, a_entry) in table.iter().enumerate() {
            let start = align_down(start, Self::N) + i * Bitfield::LEN;
            let i = start / Bitfield::LEN;

            if start > self.frames() {
                a_entry.store(HugeEntry::new());
                continue;
            }

            let entry = a_entry.load();
            let free = entry.free();
            if deep {
                // Deep recovery updates the counter
                if entry.huge() {
                    // Check that underlying bitfield is empty
                    let p = self.bitfields[i].count_zeros();
                    if p != Bitfield::LEN {
                        warn!("Invalid L2 start=0x{start:x} i{i}: h != {p}");
                        self.bitfields[i].fill(false);
                    }
                } else if free == Bitfield::LEN {
                    // Skip entirely free entries
                    // This is possible because the counter is decremented first
                    // for allocations and afterwards for frees
                    frames += Bitfield::LEN;
                } else {
                    // Check if partially filled bitfield has the same free count
                    let p = self.bitfields[i].count_zeros();
                    if free != p {
                        warn!("Invalid L2 start=0x{start:x} i{i}: {free} != {p}");
                        a_entry.store(HugeEntry::new_free(p));
                    }
                    frames += p;
                }
            } else {
                frames += free;
            }
        }

        Ok(frames)
    }

    /// Try allocating a new `frame` in the [LowerAlloc::N] sized chunk at `start`.
    pub fn get(&self, start: usize, order: usize) -> Result<usize> {
        debug_assert!(order <= Self::MAX_ORDER);

        match order {
            Self::MAX_ORDER => self.get_max(start),
            Self::HUGE_ORDER => self.get_huge(start),
            _ => self.get_small(start, order),
        }
    }

    /// Free single frame
    pub fn put(&self, frame: usize, order: usize) -> Result<()> {
        debug_assert!(order <= Self::MAX_ORDER);
        debug_assert!(frame < self.frames());

        if order == Self::MAX_ORDER {
            self.put_max(frame)
        } else if order == Self::HUGE_ORDER {
            let i = (frame / Bitfield::LEN) % Self::HP;
            let table = &self.children[frame / Self::N];

            if let Err(old) =
                table[i].compare_exchange(HugeEntry::new_huge(), HugeEntry::new_free(Bitfield::LEN))
            {
                error!("Addr p={frame:x} o={order} {old:?}");
                Err(Error::Address)
            } else {
                Ok(())
            }
        } else {
            let i = (frame / Bitfield::LEN) % Self::HP;
            let table = &self.children[frame / Self::N];

            let old = table[i].load();
            if old.huge() {
                self.partial_put_huge(old, frame, order)
            } else if old.free() <= Bitfield::LEN - (1 << order) {
                self.put_small(frame, order)
            } else {
                error!("Addr p={frame:x} o={order} {old:?}");
                Err(Error::Address)
            }
        }
    }

    /// Returns if the frame is free. This might be racy!
    pub fn is_free(&self, frame: usize, order: usize) -> bool {
        debug_assert!(frame % (1 << order) == 0);
        if order > Self::MAX_ORDER || frame + (1 << order) > self.frames() {
            return false;
        }

        if order > Bitfield::ORDER {
            // multiple huge frames
            let i = (frame / Bitfield::LEN) % Self::HP;
            self.table_pair(frame)[i / 2]
                .load()
                .all(|e| e.free() == Bitfield::LEN)
        } else {
            let table = &self.children[frame / Self::N];
            let i = (frame / Bitfield::LEN) % Self::HP;
            let entry = table[i].load();

            if entry.free() < (1 << order) {
                false
            } else if entry.free() == Bitfield::LEN {
                true
            } else {
                let bitfield = &self.bitfields[frame / Bitfield::LEN];
                bitfield.is_zero(frame % Bitfield::LEN, order)
            }
        }
    }

    /// Debug function, returning the number of allocated frames and performing internal checks.
    #[allow(unused)]
    pub fn allocated_frames(&self) -> usize {
        let mut frames = self.frames();
        for table in &*self.children {
            for entry in table.iter() {
                frames -= entry.load().free();
            }
        }
        frames
    }

    /// Debug function returning number of free frames in each order 9 chunk
    pub fn for_each_huge_frame<F: FnMut(PFN, usize)>(&self, mut f: F) {
        for (ti, table) in self.children.iter().enumerate() {
            for (ci, child) in table.iter().enumerate() {
                f(self.begin().off(ti * Self::HP + ci), child.load().free())
            }
        }
    }
}

impl Lower {
    /// Returns the table with pair entries that can be updated at once.
    fn table_pair(&self, frame: usize) -> &[Atom<HugePair>; Self::HP / 2] {
        let table = &self.children[frame / Self::N];
        unsafe { &*table.as_ptr().cast() }
    }

    fn free_all(&self) {
        // Init tables
        let (last, tables) = self.children.split_last().unwrap();
        // Table is fully included in the memory range
        for table in tables {
            table.atomic_fill(HugeEntry::new_free(Bitfield::LEN));
        }
        // Table is only partially included in the memory range
        for (i, entry) in last.iter().enumerate() {
            let frame = tables.len() * Self::N + i * Bitfield::LEN;
            let free = self.frames().saturating_sub(frame).min(Bitfield::LEN);
            entry.store(HugeEntry::new_free(free));
        }

        // Init bitfields
        let last_i = self.frames() / Bitfield::LEN;
        let (included, mut remainder) = self.bitfields.split_at(last_i);
        // Bitfield is fully included in the memory range
        for bitfield in included {
            bitfield.fill(false);
        }
        // Bitfield might be only partially included in the memory range
        if let Some((last, excluded)) = remainder.split_first() {
            let end = self.frames() - included.len() * Bitfield::LEN;
            debug_assert!(end <= Bitfield::LEN);
            last.set(0..end, false);
            last.set(end..Bitfield::LEN, true);
            remainder = excluded;
        }
        // Not part of the final memory range
        for bitfield in remainder {
            bitfield.fill(true);
        }
    }

    fn reserve_all(&self) {
        // Init table
        let (last, tables) = self.children.split_last().unwrap();
        // Table is fully included in the memory range
        for table in tables {
            table.atomic_fill(HugeEntry::new_huge());
        }
        // Table is only partially included in the memory range
        let last_i = (self.frames() / Bitfield::LEN) - tables.len() * Self::HP;
        let (included, remainder) = last.split_at(last_i);
        for entry in included {
            entry.store(HugeEntry::new_huge());
        }
        // Remainder is allocated as small frames
        for entry in remainder {
            entry.store(HugeEntry::new_free(0));
        }

        // Init bitfields
        let last_i = self.frames() / Bitfield::LEN;
        let (included, remainder) = self.bitfields.split_at(last_i);
        // Bitfield is fully included in the memory range
        for bitfield in included {
            bitfield.fill(false);
        }
        // Bitfield might be only partially included in the memory range
        for bitfield in remainder {
            bitfield.fill(true);
        }
    }

    /// Allocate frames up to order 8
    fn get_small(&self, start: usize, order: usize) -> Result<usize> {
        debug_assert!(order < Bitfield::ORDER);

        let first_bf_i = align_down(start / Bitfield::LEN, Self::HP);
        let start_bf_e = (start / Bitfield::ENTRY_BITS) % Bitfield::ENTRIES;
        let table = &self.children[start / Self::N];
        let offset = (start / Bitfield::LEN) % Self::HP;

        for j in 0..Self::HP {
            let i = (j + offset) % Self::HP;

            if table[i].fetch_update(|v| v.dec(1 << order)).is_ok() {
                let bf_i = first_bf_i + i;
                // start with the previous bitfield entry
                let bf_e = if j == 0 { start_bf_e } else { 0 };

                if let Ok(offset) = self.bitfields[bf_i].set_first_zeros(bf_e, order) {
                    return Ok(bf_i * Bitfield::LEN + offset);
                }

                // Revert conter
                if let Err(_) = table[i].fetch_update(|v| v.inc(Bitfield::LEN, 1 << order)) {
                    error!("Undo failed");
                    return Err(Error::Corruption);
                }
            }
        }

        info!("Nothing found o={order}");
        Err(Error::Memory)
    }

    /// Allocate huge frame
    fn get_huge(&self, start: usize) -> Result<usize> {
        let table = &self.children[start / Self::N];
        let offset = (start / Bitfield::LEN) % Self::HP;

        for i in 0..Self::HP {
            let i = (offset + i) % Self::HP;
            if let Ok(_) = table[i].fetch_update(|v| v.mark_huge(Bitfield::LEN)) {
                return Ok(align_down(start, Self::N) + i * Bitfield::LEN);
            }
        }

        info!("Nothing found o=9");
        Err(Error::Memory)
    }

    /// Allocate multiple huge frames
    fn get_max(&self, start: usize) -> Result<usize> {
        let table_pair = self.table_pair(start);
        let offset = ((start / Bitfield::LEN) % Self::HP) / 2;

        for i in 0..Self::HP / 2 {
            let i = (offset + i) % (Self::HP / 2);
            if let Ok(_) = table_pair[i].fetch_update(|v| v.map(|v| v.mark_huge(Bitfield::LEN))) {
                return Ok(align_down(start, Self::N) + 2 * i * Bitfield::LEN);
            }
        }

        info!("Nothing found o=10");
        Err(Error::Memory)
    }

    fn put_small(&self, frame: usize, order: usize) -> Result<()> {
        debug_assert!(order < Self::HUGE_ORDER);

        let bitfield = &self.bitfields[frame / Bitfield::LEN];
        let i = frame % Bitfield::LEN;
        if bitfield.toggle(i, order, true).is_err() {
            error!("L1 put failed i{i} p={frame}");
            return Err(Error::Address);
        }

        let table = &self.children[frame / Self::N];
        let i = (frame / Bitfield::LEN) % Self::HP;
        if let Err(entry) = table[i].fetch_update(|v| v.inc(Bitfield::LEN, 1 << order)) {
            error!("Inc failed i{i} p={frame} {entry:?}");
            return Err(Error::Corruption);
        }

        Ok(())
    }

    pub fn put_max(&self, frame: usize) -> Result<()> {
        let table_pair = self.table_pair(frame);
        let i = ((frame / Bitfield::LEN) % Self::HP) / 2;

        if let Err(old) = table_pair[i].compare_exchange(
            HugePair(HugeEntry::new_huge(), HugeEntry::new_huge()),
            HugePair(
                HugeEntry::new_free(Bitfield::LEN),
                HugeEntry::new_free(Bitfield::LEN),
            ),
        ) {
            error!("Addr {frame} o={} {old:?} i={i}", Self::MAX_ORDER);
            Err(Error::Address)
        } else {
            Ok(())
        }
    }

    fn partial_put_huge(&self, old: HugeEntry, frame: usize, order: usize) -> Result<()> {
        info!("partial free of huge frame {frame:x} o={order}");
        let i = (frame / Bitfield::LEN) % Self::HP;
        let table = &self.children[frame / Self::N];
        let bitfield = &self.bitfields[frame / Bitfield::LEN];

        // Try filling the whole bitfield
        if bitfield.toggle(0, Bitfield::ORDER, false).is_ok() {
            if table[i].compare_exchange(old, HugeEntry::new()).is_err() {
                error!("Failed partial clear");
                return Err(Error::Corruption);
            }
        }
        // Wait for parallel partial_put_huge to finish
        else if !spin_wait(CAS_RETRIES, || !table[i].load().huge()) {
            error!("Exceeding retries");
            return Err(Error::Corruption);
        }

        self.put_small(frame, order)
    }

    #[allow(dead_code)]
    pub fn dump(&self, start: usize) {
        let mut out = String::new();
        writeln!(out, "Dumping pt {}", start / Self::N).unwrap();
        let table = &self.children[start / Self::N];
        for (i, entry) in table.iter().enumerate() {
            let start = align_down(start, Self::N) + i * Bitfield::LEN;
            if start > self.frames() {
                break;
            }

            let entry = entry.load();
            let indent = 4;
            let bitfield = &self.bitfields[start / Bitfield::LEN];
            writeln!(out, "{:indent$}l2 i={i}: {entry:?}\t{bitfield:?}", "").unwrap();
        }
        warn!("{out}");
    }
}

impl Drop for Lower {
    fn drop(&mut self) {
        if self.persistent {
            // The chunks are part of the allocators managed memory
            Box::leak(core::mem::take(&mut self.bitfields));
            Box::leak(core::mem::take(&mut self.children));
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod test {
    use std::sync::Barrier;

    use alloc::vec::Vec;
    use log::warn;

    use super::Bitfield;
    use crate::frame::{Frame, PFN, PT_LEN};
    use crate::lower::Lower;
    use crate::thread;
    use crate::util::{logging, WyRand};
    use crate::Init;

    type Allocator = Lower;

    #[test]
    fn alloc_normal() {
        logging();

        const FRAMES: usize = 4 * Allocator::N;
        let lower = Allocator::new(2, PFN(0), FRAMES, Init::Volatile, true);
        lower.get(0, 0).unwrap();

        thread::parallel(0..2, |t| {
            thread::pin(t);

            let frame = lower.get(0, 0).unwrap();
            assert!(frame < FRAMES);
        });

        assert_eq!(lower.children[0][0].load().free(), Bitfield::LEN - 3);
        assert_eq!(lower.bitfields[0].count_zeros(), Bitfield::LEN - 3);
    }

    #[test]
    fn alloc_first() {
        logging();

        let lower = Allocator::new(2, PFN(0), 4 * Allocator::N, Init::Volatile, true);

        thread::parallel(0..2, |t| {
            thread::pin(t);

            lower.get(0, 0).unwrap();
        });

        let entry2 = lower.children[0][0].load();
        assert_eq!(entry2.free(), Bitfield::LEN - 2);
        assert_eq!(lower.bitfields[0].count_zeros(), Bitfield::LEN - 2);
    }

    #[test]
    fn alloc_last() {
        logging();

        let lower = Allocator::new(2, PFN(0), 4 * Allocator::N, Init::Volatile, true);

        for _ in 0..Bitfield::LEN - 1 {
            lower.get(0, 0).unwrap();
        }

        thread::parallel(0..2, |t| {
            thread::pin(t);

            lower.get(0, 0).unwrap();
        });

        let table = &lower.children[0];
        assert_eq!(table[0].load().free(), 0);
        assert_eq!(table[1].load().free(), Bitfield::LEN - 1);
        assert_eq!(lower.bitfields[1].count_zeros(), Bitfield::LEN - 1);
    }

    #[test]
    fn free_normal() {
        logging();

        let mut frames = [0; 2];

        let lower = Allocator::new(2, PFN(0), 4 * Allocator::N, Init::Volatile, true);

        frames[0] = lower.get(0, 0).unwrap();
        frames[1] = lower.get(0, 0).unwrap();

        thread::parallel(0..2, |t| {
            thread::pin(t);

            lower.put(frames[t as usize], 0).unwrap();
        });

        assert_eq!(lower.children[0][0].load().free(), Bitfield::LEN);
    }

    #[test]
    fn free_last() {
        logging();

        let mut frames = [0; Bitfield::LEN];

        let lower = Allocator::new(2, PFN(0), 4 * Allocator::N, Init::Volatile, true);

        for frame in &mut frames {
            *frame = lower.get(0, 0).unwrap();
        }

        thread::parallel(0..2, |t| {
            thread::pin(t);

            lower.put(frames[t as usize], 0).unwrap();
        });

        let table = &lower.children[0];
        assert_eq!(table[0].load().free(), 2);
        assert_eq!(lower.bitfields[0].count_zeros(), 2);
    }

    #[test]
    fn realloc_last() {
        logging();

        let mut frames = [0; Bitfield::LEN];

        let lower = Allocator::new(2, PFN(0), 4 * Allocator::N, Init::Volatile, true);

        for frame in &mut frames[..Bitfield::LEN - 1] {
            *frame = lower.get(0, 0).unwrap();
        }

        std::thread::scope(|s| {
            s.spawn(|| {
                thread::pin(0);

                lower.get(0, 0).unwrap();
            });
            thread::pin(1);

            lower.put(frames[0], 0).unwrap();
        });

        let table = &lower.children[0];
        if table[0].load().free() == 1 {
            assert_eq!(lower.bitfields[0].count_zeros(), 1);
        } else {
            // Table entry skipped
            assert_eq!(table[0].load().free(), 2);
            assert_eq!(lower.bitfields[0].count_zeros(), 2);
            assert_eq!(table[1].load().free(), Bitfield::LEN - 1);
            assert_eq!(lower.bitfields[1].count_zeros(), Bitfield::LEN - 1);
        }
    }

    #[test]
    fn alloc_normal_large() {
        logging();

        const FRAMES: usize = 4 * Allocator::N;
        let lower = Allocator::new(2, PFN(0), FRAMES, Init::Volatile, true);
        lower.get(0, 0).unwrap();

        thread::parallel(0..2, |t| {
            thread::pin(t);

            let order = t + 1; // order 1 and 2
            let frame = lower.get(0, order).unwrap();
            assert!(frame < FRAMES);
        });

        let allocated = 1 + 2 + 4;
        assert_eq!(
            lower.children[0][0].load().free(),
            Bitfield::LEN - allocated
        );
        assert_eq!(lower.bitfields[0].count_zeros(), Bitfield::LEN - allocated);
    }

    #[test]
    fn free_normal_large() {
        logging();

        let mut frames = [0; 2];

        let lower = Allocator::new(2, PFN(0), 4 * Allocator::N, Init::Volatile, true);

        frames[0] = lower.get(0, 1).unwrap();
        frames[1] = lower.get(0, 2).unwrap();

        assert_eq!(lower.children[0][0].load().free(), Bitfield::LEN - 2 - 4);

        thread::parallel(0..2, |t| {
            thread::pin(t);

            lower.put(frames[t as usize], t + 1).unwrap();
        });

        assert_eq!(lower.children[0][0].load().free(), Bitfield::LEN);
    }

    #[test]
    fn different_orders() {
        logging();

        const MAX_ORDER: usize = Allocator::MAX_ORDER;

        thread::pin(0);
        let lower = Allocator::new(1, PFN(0), 4 * Allocator::N, Init::Volatile, true);

        assert_eq!(lower.allocated_frames(), 0);

        let mut rng = WyRand::new(42);

        let mut num_frames = 0;
        let mut frames = Vec::new();
        for order in 0..=MAX_ORDER {
            for _ in 0..1usize << (MAX_ORDER - order) {
                frames.push((order, 0));
                num_frames += 1 << order;
            }
        }
        rng.shuffle(&mut frames);
        warn!("allocate {num_frames} frames up to order {MAX_ORDER}");

        for (order, frame) in &mut frames {
            *frame = lower.get(0, *order).unwrap();
        }

        assert_eq!(lower.allocated_frames(), num_frames);

        for (order, frame) in &frames {
            lower.put(*frame, *order).unwrap();
        }

        assert_eq!(lower.allocated_frames(), 0);
    }

    #[test]
    fn init_reserved() {
        logging();

        const MAX_ORDER: usize = Allocator::MAX_ORDER;

        let num_max_frames = (Allocator::N - 1) / (1 << MAX_ORDER);

        let lower = Allocator::new(1, PFN(0), Allocator::N - 1, Init::Volatile, false);

        assert_eq!(lower.allocated_frames(), Allocator::N - 1);

        for i in 0..num_max_frames {
            lower.put(i * (1 << MAX_ORDER), MAX_ORDER).unwrap();
        }

        assert_eq!(lower.allocated_frames(), (1 << MAX_ORDER) - 1);
    }

    #[test]
    fn partial_put_huge() {
        logging();

        let lower = Allocator::new(1, PFN(0), Allocator::N - 1, Init::Volatile, false);

        assert_eq!(lower.allocated_frames(), Allocator::N - 1);

        lower.put(0, 0).unwrap();

        assert_eq!(lower.allocated_frames(), Allocator::N - 2);
    }

    #[test]
    #[ignore]
    fn rand_realloc_first() {
        logging();

        const THREADS: usize = 6;
        let buffer = vec![Frame::new(); 2 * THREADS * PT_LEN * PT_LEN];

        for _ in 0..8 {
            let lower = Allocator::new(
                THREADS,
                PFN::from_ptr(buffer.as_ptr()),
                buffer.len(),
                Init::Overwrite,
                true,
            );
            assert_eq!(lower.allocated_frames(), 0);

            let barrier = Barrier::new(THREADS);
            thread::parallel(0..THREADS, |t| {
                thread::pin(t);
                barrier.wait();

                let mut frames = [0; 4];
                for p in &mut frames {
                    *p = lower.get(0, 0).unwrap();
                }
                frames.reverse();
                for p in frames {
                    lower.put(p, 0).unwrap();
                }
            });

            assert_eq!(lower.allocated_frames(), 0);
        }
    }

    #[test]
    #[ignore]
    fn rand_realloc_last() {
        logging();

        const THREADS: usize = 6;
        let mut frames = [0; PT_LEN];
        let buffer = vec![Frame::new(); 2 * THREADS * PT_LEN * PT_LEN];

        for _ in 0..8 {
            let lower = Allocator::new(
                THREADS,
                PFN::from_ptr(buffer.as_ptr()),
                buffer.len(),
                Init::Overwrite,
                true,
            );
            assert_eq!(lower.allocated_frames(), 0);

            for frame in &mut frames[..PT_LEN - 3] {
                *frame = lower.get(0, 0).unwrap();
            }

            let barrier = Barrier::new(THREADS);
            thread::parallel(0..THREADS, |t| {
                thread::pin(t);
                barrier.wait();

                if t < THREADS / 2 {
                    lower.put(frames[t], 0).unwrap();
                } else {
                    lower.get(0, 0).unwrap();
                }
            });

            assert_eq!(lower.allocated_frames(), PT_LEN - 3);
        }
    }
}
