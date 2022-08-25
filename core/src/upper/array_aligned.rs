use core::fmt;
use core::ops::Index;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::vec::Vec;
use log::{error, info, warn};

use super::{Alloc, Local, CAS_RETRIES, MAGIC, MAX_PAGES};
use crate::atomic::{ANode, AStack, AStackDbg, Atomic, Next};
use crate::entry::Entry3;
use crate::lower::LowerAlloc;
use crate::util::{align_down, Page};
use crate::{Error, Result};

/// Non-Volatile global metadata
struct Meta {
    magic: AtomicUsize,
    pages: AtomicUsize,
    active: AtomicUsize,
}
const _: () = assert!(core::mem::size_of::<Meta>() <= Page::SIZE);

/// This allocator splits its memory range into 1G chunks.
/// Giant pages are directly allocated in it.
/// For smaller pages, however, the 1G chunk is handed over to the
/// lower allocator, managing these smaller allocations.
/// These 1G chunks are, due to the inner workins of the lower allocator,
/// called 1G *subtrees*.
///
/// This allocator uses a cache-line aligned array to store the subtrees
/// (level 3 entries).
/// The subtree reservation is speed up using free lists for
/// empty and partially empty subtrees.
/// These free lists are implemented as atomic linked lists with their next
/// pointers stored inside the level 3 entries.
///
/// This volatile shared metadata is rebuild on boot from
/// the persistent metadata of the lower allocator.
#[repr(align(64))]
pub struct ArrayAligned<A: Entry, L: LowerAlloc> {
    /// Pointer to the metadata page at the end of the allocators persistent memory range
    meta: *mut Meta,
    /// Array of level 3 entries, the roots of the 1G subtrees, the lower alloc manages
    subtrees: Box<[A]>,
    /// CPU local data
    local: Box<[Local<0>]>,
    /// Metadata of the lower alloc
    lower: L,

    /// List of idx to subtrees that are not allocated at all
    empty: AStack<Entry3>,
    /// List of idx to subtrees that are partially allocated with small pages
    partial: AStack<Entry3>,
}

pub trait Entry: Sized {
    fn new(v: Atomic<Entry3>) -> Self;
    fn as_ref(&self) -> &Atomic<Entry3>;
}

/// Cache line aligned entries to prevent false-sharing.
#[repr(align(64))]
pub struct CacheAligned(Atomic<Entry3>);
impl Entry for CacheAligned {
    fn new(v: Atomic<Entry3>) -> Self {
        Self(v)
    }
    fn as_ref(&self) -> &Atomic<Entry3> {
        &self.0
    }
}

/// *Not* cache-line aligned, to test false-sharing
#[repr(transparent)]
pub struct Unaligned(Atomic<Entry3>);
impl Entry for Unaligned {
    fn new(v: Atomic<Entry3>) -> Self {
        Self(v)
    }
    fn as_ref(&self) -> &Atomic<Entry3> {
        &self.0
    }
}

impl<A: Entry, L: LowerAlloc> Index<usize> for ArrayAligned<A, L> {
    type Output = Atomic<Entry3>;

    fn index(&self, index: usize) -> &Self::Output {
        self.subtrees[index].as_ref()
    }
}

unsafe impl<A: Entry, L: LowerAlloc> Send for ArrayAligned<A, L> {}
unsafe impl<A: Entry, L: LowerAlloc> Sync for ArrayAligned<A, L> {}

impl<A: Entry, L: LowerAlloc> fmt::Debug for ArrayAligned<A, L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} {{", self.name())?;
        writeln!(
            f,
            "    memory: {:?} ({})",
            self.lower.memory(),
            self.lower.pages()
        )?;
        for (i, entry) in self.subtrees.iter().enumerate() {
            let pte = entry.as_ref().load();
            writeln!(f, "    {i:>3}: {pte:?}")?;
        }
        writeln!(f, "    empty: {:?}", AStackDbg(&self.empty, self))?;
        writeln!(f, "    partial: {:?}", AStackDbg(&self.partial, self))?;
        writeln!(f, "}}")?;
        Ok(())
    }
}

impl<A: Entry, L: LowerAlloc> Alloc for ArrayAligned<A, L> {
    #[cold]
    fn init(&mut self, cores: usize, mut memory: &mut [Page], persistent: bool) -> Result<()> {
        info!(
            "initializing c={cores} {:?} {}",
            memory.as_ptr_range(),
            memory.len()
        );
        if memory.len() < L::N * cores {
            error!("memory {} < {}", memory.len(), L::N * cores);
            return Err(Error::Memory);
        }

        if persistent {
            // Last frame is reserved for metadata
            let (m, rem) = memory.split_at_mut((memory.len() - 1).min(MAX_PAGES));
            let meta = rem[0].cast_mut::<Meta>();
            self.meta = meta;
            memory = m;
        }

        let mut local = Vec::with_capacity(cores);
        local.resize_with(cores, Local::new);
        self.local = local.into();

        // Create lower allocator
        self.lower = L::new(cores, memory, persistent);

        // Array with all pte3
        let pte3_num = self.pages().div_ceil(L::N);
        let mut pte3s = Vec::with_capacity(pte3_num);
        pte3s.resize_with(pte3_num, || A::new(Atomic::new(Entry3::new())));
        self.subtrees = pte3s.into();

        self.empty = AStack::default();
        self.partial = AStack::default();

        Ok(())
    }

    #[cold]
    fn recover(&self) -> Result<()> {
        assert!(!self.meta.is_null());
        let meta = unsafe { &mut *self.meta };

        if meta.pages.load(Ordering::SeqCst) == self.pages()
            && meta.magic.load(Ordering::SeqCst) == MAGIC
        {
            info!("recover p={}", self.pages());
            let deep = meta.active.load(Ordering::SeqCst) != 0;
            self.recover_inner(deep)?;
            meta.active.store(1, Ordering::SeqCst);
            Ok(())
        } else {
            Err(Error::Initialization)
        }
    }

    #[cold]
    fn free_all(&self) -> Result<()> {
        info!("free all p={}", self.pages());

        self.lower.free_all();

        // Add all entries to the empty list
        let pte3_num = self.pages().div_ceil(L::N);
        for i in 0..pte3_num - 1 {
            self[i].store(Entry3::empty(L::N).with_next(Next::Outside));
            self.empty.push(self, i);
        }

        // The last one may be cut off
        let max = (self.pages() - (pte3_num - 1) * L::N).min(L::N);
        self[pte3_num - 1].store(Entry3::new().with_free(max).with_next(Next::Outside));

        if max == L::N {
            self.empty.push(self, pte3_num - 1);
        } else if max > Self::ALMOST_FULL {
            self.partial.push(self, pte3_num - 1);
        }

        if !self.meta.is_null() {
            let meta = unsafe { &mut *self.meta };
            meta.pages.store(self.pages(), Ordering::SeqCst);
            meta.magic.store(MAGIC, Ordering::SeqCst);
            meta.active.store(1, Ordering::SeqCst);
        }
        Ok(())
    }

    #[cold]
    fn reserve_all(&self) -> Result<()> {
        info!("reserve all p={}", self.pages());

        self.lower.reserve_all();

        // Set all entries to zero
        let pte3_num = self.pages().div_ceil(L::N);
        for i in 0..pte3_num {
            self[i].store(Entry3::new().with_next(Next::Outside));
        }
        // Clear the lists
        self.empty.set(Entry3::default().with_next(Next::End));
        self.partial.set(Entry3::default().with_next(Next::End));

        if !self.meta.is_null() {
            let meta = unsafe { &mut *self.meta };
            meta.pages.store(self.pages(), Ordering::SeqCst);
            meta.magic.store(MAGIC, Ordering::SeqCst);
            meta.active.store(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn get(&self, core: usize, order: usize) -> Result<u64> {
        if order > L::MAX_ORDER {
            error!("invalid order: !{order} <= {}", L::MAX_ORDER);
            return Err(Error::Memory);
        }

        let start_a = &self.local[core].start;
        let mut start = start_a.load();

        if start == usize::MAX {
            start = self.reserve(order)?;
            start_a.store(start);
        } else {
            let i = start / L::N;
            if self[i].update(|v| v.dec(1 << order)).is_err() {
                start = self.reserve(order)?;
                start_a.store(start);
                if self[i].update(|v| v.toggle_reserve(false)).is_err() {
                    error!("Unreserve failed");
                    return Err(Error::Corruption);
                }
            }
        }

        // TODO: Better handle: Reserve + Failed Alloc (fragmentation) -> Search through partial...
        for _ in 0..CAS_RETRIES {
            match self.lower.get(start, order) {
                Ok(page) => {
                    // small pages
                    if order < 64usize.ilog2() as usize {
                        start_a.store(page);
                    }
                    return Ok(unsafe { self.lower.memory().start.add(page as _) } as u64);
                }
                Err(Error::Memory) => {
                    let i = start / L::N;
                    let max = (self.pages() - align_down(start, L::N)).min(L::N);
                    match self[i].update(|v| v.inc(1 << order, max)) {
                        Err(e) => {
                            error!("Counter reset failed o={order} {i}: {e:?}");
                            return Err(Error::Corruption);
                        }
                        Ok(pte3) => {
                            start_a.store(self.reserve(order)?);
                            if self[i].update(|v| v.toggle_reserve(false)).is_err() {
                                error!("Unreserve failed");
                                return Err(Error::Corruption);
                            }
                            // Add back to partial
                            let new_pages = pte3.free() + (1 << order);
                            if pte3.free() <= Self::ALMOST_FULL && new_pages > Self::ALMOST_FULL {
                                self.partial.push(self, i);
                            }
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        error!("No memory found!");
        Err(Error::Memory)
    }

    fn put(&self, _core: usize, addr: u64, order: usize) -> Result<()> {
        if order > L::MAX_ORDER {
            error!("invalid order: !{order} <= {}", L::MAX_ORDER);
            return Err(Error::Memory);
        }
        let num_pages = 1 << order;

        if addr % (num_pages * Page::SIZE) as u64 != 0
            || !self.lower.memory().contains(&(addr as _))
        {
            error!(
                "invalid addr 0x{addr:x} r={:?} o={order}",
                self.lower.memory()
            );
            return Err(Error::Address);
        }

        let page = unsafe { (addr as *const Page).offset_from(self.lower.memory().start) } as usize;

        self.lower.put(page, order)?;

        let i = page / L::N;
        // The last page table might have fewer pages
        let max = (self.pages() - i * L::N).min(L::N);

        match self[i].update(|v| v.inc(num_pages, max)) {
            Ok(pte3) => {
                if !pte3.reserved() {
                    // Add back to partial
                    let new_pages = pte3.free() + num_pages;
                    if pte3.free() <= Self::ALMOST_FULL && new_pages > Self::ALMOST_FULL {
                        self.partial.push(self, i);
                    }
                }
                Ok(())
            }
            Err(pte) => {
                error!("inc failed i{i}: {pte:?} o={order}");
                Err(Error::Corruption)
            }
        }
    }

    fn pages(&self) -> usize {
        self.lower.pages()
    }

    fn pages_needed(&self, cores: usize) -> usize {
        L::N * cores
    }

    #[cold]
    fn dbg_for_each_huge_page(&self, f: fn(usize)) {
        self.lower.dbg_for_each_huge_page(f)
    }

    #[cold]
    fn dbg_free_pages(&self) -> usize {
        let mut pages = 0;
        for i in 0..self.pages().div_ceil(L::N) {
            let pte = self[i].load();
            pages += pte.free();
        }
        pages
    }
}

impl<A: Entry, L: LowerAlloc> Drop for ArrayAligned<A, L> {
    fn drop(&mut self) {
        if !self.meta.is_null() {
            let meta = unsafe { &*self.meta };
            meta.active.store(0, Ordering::SeqCst);
        }
    }
}

impl<A: Entry, L: LowerAlloc> Default for ArrayAligned<A, L> {
    fn default() -> Self {
        Self {
            meta: null_mut(),
            lower: L::default(),
            local: Box::new([]),
            subtrees: Box::new([]),
            empty: AStack::default(),
            partial: AStack::default(),
        }
    }
}

impl<A: Entry, L: LowerAlloc> ArrayAligned<A, L> {
    const ALMOST_FULL: usize = 1 << L::MAX_ORDER;

    /// Recover the allocator from NVM after reboot.
    /// If `deep` then the level 1 page tables are traversed and diverging counters are corrected.
    #[cold]
    fn recover_inner(&self, deep: bool) -> Result<usize> {
        if deep {
            warn!("Try recover crashed allocator!");
        }
        let mut total = 0;
        for i in 0..self.pages().div_ceil(L::N) {
            let page = i * L::N;
            let pages = self.lower.recover(page, deep)?;

            self[i].store(Entry3::new_table(pages, false));

            // Add to lists
            if pages == L::N {
                self.empty.push(self, i);
            } else if pages > Self::ALMOST_FULL {
                self.partial.push(self, i);
            }
            total += pages;
        }
        Ok(total)
    }

    /// Reserves a new subtree, prioritizing partially filled subtrees,
    /// and allocates a page from it in one step.
    fn reserve(&self, order: usize) -> Result<usize> {
        while let Some((i, r)) = self.partial.pop_update(self, |v| {
            // Skip empty entries
            if v.free() < L::N {
                v.dec(1 << order)
            } else {
                None
            }
        }) {
            if r.is_ok() {
                info!("reserve partial {i}");
                return Ok(i * L::N);
            }
            self.empty.push(self, i);
        }

        if let Some((i, r)) = self.empty.pop_update(self, |v| v.dec(1 << order)) {
            debug_assert!(r.is_ok());
            info!("reserve empty {i}");
            Ok(i * L::N)
        } else {
            error!("No memory");
            Err(Error::Memory)
        }
    }
}
