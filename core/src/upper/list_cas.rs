use core::fmt;
use core::ops::Index;
use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::vec::Vec;
use log::{error, info};

use super::{Alloc, Init, MIN_PAGES};
use crate::atomic::Atom;
use crate::entry::Next;
use crate::util::Page;
use crate::{Error, Result};

/// Simple volatile 4K page allocator that uses a single shared linked lists
/// protected by a ticked lock.
/// The linked list pointers are stored similar to Linux's in the struct pages.
///
/// As expected the contention on the ticket lock is very high.
#[repr(align(64))]
pub struct ListCAS {
    offset: u64,
    len: usize,
    frames: Box<[PageFrame]>,
    /// CPU local metadata
    local: Box<[LocalCounter]>,
    /// Per page metadata
    list: AtomicStack,
}

#[repr(align(64))]
struct LocalCounter {
    counter: AtomicUsize,
}
const _: () = assert!(core::mem::align_of::<LocalCounter>() == 64);

unsafe impl Send for ListCAS {}
unsafe impl Sync for ListCAS {}

impl fmt::Debug for ListCAS {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} {{", self.name())?;
        for (t, l) in self.local.iter().enumerate() {
            writeln!(f, "    L {t:>2} C={}", l.counter.load(Ordering::Relaxed))?;
        }
        writeln!(f, "}}")?;
        Ok(())
    }
}

impl Default for ListCAS {
    fn default() -> Self {
        Self {
            len: 0,
            offset: 0,
            list: AtomicStack::default(),
            local: Box::new([]),
            frames: Box::new([]),
        }
    }
}

impl Alloc for ListCAS {
    #[cold]
    fn init(
        &mut self,
        cores: usize,
        memory: &mut [Page],
        init: Init,
        free_all: bool,
    ) -> Result<()> {
        debug_assert!(init == Init::Volatile);
        info!(
            "initializing c={cores} {:?} {}",
            memory.as_ptr_range(),
            memory.len()
        );
        if memory.len() < cores * MIN_PAGES {
            error!("Not enough memory {} < {}", memory.len(), cores * MIN_PAGES);
            return Err(Error::Memory);
        }

        self.offset = memory.as_ptr() as u64 / Page::SIZE as u64;
        self.len = memory.len();

        let mut local = Vec::with_capacity(cores);
        local.resize_with(cores, || LocalCounter {
            counter: AtomicUsize::new(0),
        });
        self.local = local.into();

        let mut struct_pages = Vec::with_capacity(memory.len());
        struct_pages.resize_with(memory.len(), PageFrame::new);
        self.frames = struct_pages.into();

        if free_all {
            self.free_all()?;
        } else {
            self.reserve_all()?;
        }

        Ok(())
    }

    fn get(&self, core: usize, order: usize) -> Result<u64> {
        if order != 0 {
            error!("order {order:?} not supported");
            return Err(Error::Memory);
        }

        if let Some(pfn) = self.list.pop(self) {
            self.local[core].counter.fetch_add(1, Ordering::Relaxed);
            Ok(self.from_pfn(pfn))
        } else {
            error!("No memory");
            Err(Error::Memory)
        }
    }

    fn put(&self, core: usize, addr: u64, order: usize) -> Result<()> {
        if order != 0 {
            error!("order {order:?} not supported");
            return Err(Error::Memory);
        }
        let pfn = self.to_pfn(addr)?;

        self.list.push(self, pfn);
        self.local[core].counter.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }

    fn is_free(&self, _addr: u64, _order: usize) -> bool {
        false
    }

    fn pages(&self) -> usize {
        self.len
    }

    fn dbg_free_pages(&self) -> usize {
        self.pages()
            - self
                .local
                .iter()
                .map(|c| c.counter.load(Ordering::SeqCst))
                .sum::<usize>()
    }
}

impl ListCAS {
    #[cold]
    fn free_all(&self) -> Result<()> {
        for local in self.local.iter() {
            local.counter.store(0, Ordering::Relaxed);
        }

        // build free lists
        for i in 1..self.pages() {
            self.frames[i - 1].next.store(Next::Some(i));
        }
        self.frames[self.pages() - 1].next.store(Next::End);

        self.list.start.store(Next::Some(0));
        Ok(())
    }

    fn reserve_all(&self) -> Result<()> {
        for local in self.local.iter() {
            local
                .counter
                .store(self.pages() / self.local.len(), Ordering::Relaxed);
        }
        self.list.start.store(Next::End);
        Ok(())
    }

    #[inline]
    fn to_pfn(&self, addr: u64) -> Result<usize> {
        if addr % Page::SIZE as u64 != 0 {
            return Err(Error::Address);
        }
        let off = addr / (Page::SIZE as u64);
        if let Some(pfn) = off.checked_sub(self.offset) {
            if (pfn as usize) < self.len {
                Ok(pfn as _)
            } else {
                Err(Error::Address)
            }
        } else {
            Err(Error::Address)
        }
    }
    #[inline]
    fn from_pfn(&self, pfn: usize) -> u64 {
        debug_assert!(pfn < self.len);
        (self.offset + pfn as u64) * Page::SIZE as u64
    }
}

impl Index<usize> for ListCAS {
    type Output = Atom<Next>;

    fn index(&self, index: usize) -> &Self::Output {
        &self.frames[index].next
    }
}

/// Representing Linux `struct page`
#[repr(align(64))]
struct PageFrame {
    /// Next page frame number
    next: Atom<Next>,
}
const _: () = assert!(core::mem::align_of::<PageFrame>() == 64);

impl PageFrame {
    fn new() -> Self {
        Self {
            next: Atom::new(Next::Outside),
        }
    }
}

/// Simple atomic stack with atomic entries.
/// It is constructed over an already existing fixed size buffer.
#[repr(align(64))] // Just to be sure
pub struct AtomicStack {
    start: Atom<Next>,
}

impl Default for AtomicStack {
    fn default() -> Self {
        Self {
            start: Atom::new(Next::End),
        }
    }
}

impl AtomicStack {
    /// Pushes the element at `idx` to the front of the stack.
    pub fn push<B>(&self, buf: &B, idx: usize)
    where
        B: Index<usize, Output = Atom<Next>>,
    {
        let mut prev_elem = Next::Outside;
        let mut prev = self.start.load();
        let elem = &buf[idx];
        loop {
            if elem.compare_exchange(prev_elem, prev).is_err() {
                error!("invalid list element");
                panic!()
            }

            // CAS weak is important for fetch-update!
            match self.start.compare_exchange_weak(prev, Next::Some(idx)) {
                Ok(_) => return,
                Err(s) => {
                    prev_elem = prev;
                    prev = s;
                }
            }
        }
    }

    /// Poping the first element and updating it in place.
    pub fn pop<B>(&self, buf: &B) -> Option<usize>
    where
        B: Index<usize, Output = Atom<Next>>,
    {
        let mut prev = self.start.load();
        loop {
            let idx = prev.some()?;
            let next = buf[idx].load();
            // CAS weak is important for fetch-update!
            match self.start.compare_exchange_weak(prev, next) {
                Ok(old) => {
                    let i = old.some()?;
                    if buf[i].compare_exchange(next, Next::Outside).is_err() {
                        error!("invalid list element");
                        panic!();
                    }
                    return Some(i);
                }
                Err(s) => prev = s,
            }
        }
    }
}

/// Debug printer for the [AStack].
#[allow(dead_code)]
pub struct AtomicStackDbg<'a, B>(pub &'a AtomicStack, pub &'a B)
where
    B: Index<usize, Output = Atom<Next>>;

impl<'a, B> fmt::Debug for AtomicStackDbg<'a, B>
where
    B: Index<usize, Output = Atom<Next>>,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut dbg = f.debug_list();

        if let Next::Some(mut i) = self.0.start.load() {
            let mut ended = false;
            for _ in 0..1000 {
                dbg.entry(&i);
                let elem = self.1[i].load();
                if let Next::Some(next) = elem {
                    if i == next {
                        break;
                    }
                    i = next;
                } else {
                    ended = true;
                    break;
                }
            }
            if !ended {
                error!("Circular List!");
            }
        }

        dbg.finish()
    }
}

#[cfg(test)]
mod test {
    use core::hint::black_box;
    use core::sync::atomic::AtomicU64;
    use std::sync::Barrier;

    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use log::{info, warn};

    use crate::atomic::Atom;
    use crate::mmap::test_mapping;
    use crate::table::PT_LEN;
    use crate::upper::list_cas::{AtomicStack, AtomicStackDbg, Next};
    use crate::upper::{Alloc, Init};
    use crate::util::{self, logging, Page};
    use crate::{thread, Error};

    use super::ListCAS;

    type Allocator = ListCAS;

    #[test]
    fn simple() {
        logging();
        // 8GiB
        const MEM_SIZE: usize = 8 << 30;
        let mut mapping = test_mapping(0x1000_0000_0000, MEM_SIZE / Page::SIZE).unwrap();

        info!("mmap {MEM_SIZE} bytes at {:?}", mapping.as_ptr());

        let alloc = Arc::new({
            let mut a = Allocator::default();
            a.init(1, &mut mapping, Init::Volatile, true).unwrap();
            a
        });

        assert_eq!(alloc.dbg_free_pages(), alloc.pages());

        warn!("start alloc...");
        let small = alloc.get(0, 0).unwrap();

        assert_eq!(alloc.dbg_allocated_pages(), 1, "{alloc:?}");
        warn!("stress test...");

        // Stress test
        let mut pages = Vec::new();
        loop {
            match alloc.get(0, 0) {
                Ok(page) => pages.push(page),
                Err(Error::Memory) => break,
                Err(e) => panic!("{e:?}"),
            }
        }

        warn!("allocated {}", 1 + pages.len());
        warn!("check...");

        assert_eq!(alloc.dbg_allocated_pages(), 1 + pages.len());
        assert_eq!(alloc.dbg_allocated_pages(), alloc.pages());
        pages.sort_unstable();

        // Check that the same page was not allocated twice
        for i in 0..pages.len() - 1 {
            let p1 = pages[i];
            let p2 = pages[i + 1];
            assert!(mapping.as_ptr_range().contains(&(p1 as _)));
            assert!(p1 != p2);
        }

        warn!("realloc...");

        // Free some
        const FREE_NUM: usize = PT_LEN * PT_LEN - 10;
        for page in &pages[..FREE_NUM] {
            alloc.put(0, *page, 0).unwrap();
        }

        assert_eq!(
            alloc.dbg_allocated_pages(),
            1 + pages.len() - FREE_NUM,
            "{alloc:?}"
        );

        // Realloc
        for page in &mut pages[..FREE_NUM] {
            *page = alloc.get(0, 0).unwrap();
        }

        warn!("free...");

        alloc.put(0, small, 0).unwrap();
        // Free all
        for page in &pages {
            alloc.put(0, *page, 0).unwrap();
        }

        assert_eq!(alloc.dbg_allocated_pages(), 0);
    }

    #[test]
    fn atomic_stack() {
        util::logging();
        const DATA_V: Atom<Next> = Atom::raw(AtomicU64::new(u64::MAX));
        const N: usize = 640;
        let data: [Atom<Next>; N] = [DATA_V; N];

        let stack = AtomicStack::default();
        stack.push(&data, 0);
        stack.push(&data, 1);

        warn!("{:?}", AtomicStackDbg(&stack, &data));

        assert_eq!(stack.pop(&data), Some(1));
        assert_eq!(stack.pop(&data), Some(0));
        assert_eq!(stack.pop(&data), None);

        // Stress test
        warn!("parallel:");

        const THREADS: usize = 6;
        const I: usize = N / THREADS;
        let barrier = Barrier::new(THREADS);
        thread::parallel(0..THREADS, |t| {
            thread::pin(t);
            let mut idx: [usize; I] = [0; I];
            for i in 0..I {
                idx[i] = t * I + i;
            }
            barrier.wait();

            for _ in 0..1000 {
                for &i in &idx {
                    stack.push(&data, i);
                }
                idx = black_box(idx);
                for (i, &a) in idx.iter().enumerate() {
                    for (j, &b) in idx.iter().enumerate() {
                        assert!(i == j || a != b);
                    }
                }
                for i in &mut idx {
                    *i = stack.pop(&data).unwrap();
                }
            }
        });
        assert_eq!(stack.pop(&data), None);
    }
}
