use core::fmt::Write;
use core::ops::Range;

use alloc::string::String;
use log::{error, warn, info};

use crate::entry::{Entry2, Entry2Pair};
use crate::table::{ATable, Bitfield, Mapping};
use crate::upper::CAS_RETRIES;
use crate::util::{align_up, div_ceil, Page};
use crate::{Error, Result};

use super::LowerAlloc;

/// Level 2 page allocator.
/// ```text
/// NVRAM: [ Pages | PT1s + padding | PT2s | Meta ]
/// ```
#[derive(Default, Debug)]
pub struct CacheLower<const T2N: usize> {
    pub begin: usize,
    pub pages: usize,
}

impl<const T2N: usize> LowerAlloc for CacheLower<T2N>
where
    [(); T2N / 2]:,
{
    const MAPPING: Mapping<2> = Mapping([Bitfield::ORDER, ATable::<Entry2, T2N>::ORDER]);
    const HUGE_ORDER: usize = Self::MAPPING.order(1);
    const MAX_ORDER: usize = Self::HUGE_ORDER + 1;

    fn new(_cores: usize, memory: &mut [Page]) -> Self {
        let s1 = Self::MAPPING.num_pts(1, memory.len()) * Bitfield::SIZE;
        let s1 = align_up(s1, ATable::<Entry2, T2N>::SIZE); // correct alignment
        let s2 = Self::MAPPING.num_pts(2, memory.len()) * ATable::<Entry2, T2N>::SIZE;
        let pages = div_ceil(s1 + s2, Page::SIZE);
        Self {
            begin: memory.as_ptr() as usize,
            // level 1 and 2 tables are stored at the end of the NVM
            pages: memory.len() - pages,
        }
    }

    fn pages(&self) -> usize {
        self.pages
    }

    fn memory(&self) -> Range<*const Page> {
        self.begin as *const Page..(self.begin + self.pages * Page::SIZE) as *const Page
    }

    fn clear(&self) {
        // Init pt2
        for i in 0..Self::MAPPING.num_pts(2, self.pages) {
            let pt2 = self.pt2(i * Self::MAPPING.span(2));
            if i + 1 < Self::MAPPING.num_pts(2, self.pages) {
                pt2.fill(Entry2::new().with_free(Self::MAPPING.span(1)));
            } else {
                for j in 0..Self::MAPPING.len(2) {
                    let page = i * Self::MAPPING.span(2) + j * Self::MAPPING.span(1);
                    let max = Self::MAPPING.span(1).min(self.pages.saturating_sub(page));
                    pt2.set(j, Entry2::new().with_free(max));
                }
            }
        }
        // Init pt1
        for i in 0..Self::MAPPING.num_pts(1, self.pages) {
            let pt1 = self.pt1(i * Self::MAPPING.span(1));

            if i + 1 < Self::MAPPING.num_pts(1, self.pages) {
                pt1.fill(false);
            } else {
                for j in 0..Self::MAPPING.len(1) {
                    let page = i * Self::MAPPING.span(1) + j;
                    pt1.set(j, page >= self.pages);
                }
            }
        }
    }

    fn recover(&self, start: usize, deep: bool) -> Result<usize> {
        let mut pages = 0;

        let pt = self.pt2(start);
        for i in 0..Self::MAPPING.len(2) {
            let start = Self::MAPPING.page(2, start, i);
            if start > self.pages {
                pt.set(i, Entry2::new());
            }

            let pte = pt.get(i);
            if pte.page() {
                pages += pte.free()
            } else if deep && pte.free() > 0 {
                let p = self.recover_l1(start);
                if pte.free() != p {
                    warn!("Invalid PTE2 start=0x{start:x} i{i}: {} != {p}", pte.free());
                    pt.set(i, pte.with_free(p));
                }
                pages += p;
            } else {
                pages += pte.free();
            }
        }

        Ok(pages)
    }

    fn get(&self, start: usize, order: usize) -> Result<usize> {
        debug_assert!(order <= Self::MAX_ORDER);

        if order > Self::HUGE_ORDER {
            self.get_max(start)
        } else if 1 << order > u64::BITS {
            self.get_huge(start)
        } else {
            self.get_small(start, order)
        }
    }

    /// Free single page and returns if the page was huge
    fn put(&self, page: usize, order: usize) -> Result<()> {
        debug_assert!(order <= Self::MAX_ORDER);
        debug_assert!(page < self.pages);
        stop!();

        if order > Self::HUGE_ORDER {
            return self.put_max(page, order);
        }

        let pt2 = self.pt2(page);
        let i2 = Self::MAPPING.idx(2, page);
        if (1 << order) <= u64::BITS {
            let old = pt2.get(i2);
            if !old.page() && old.free() <= Self::MAPPING.span(1) - (1 << order) {
                self.put_small(page, order)
            } else {
                error!("Addr p={page:x} o={order} {old:?}");
                Err(Error::Address)
            }
        } else {
            // try free huge
            if let Err(old) = pt2.cas(
                i2,
                Entry2::new_page(),
                Entry2::new_free(Self::MAPPING.span(1)),
            ) {
                error!("Addr {page:x} o={order} {old:?}");
                Err(Error::Address)
            } else {
                Ok(())
            }
        }
    }

    fn dbg_allocated_pages(&self) -> usize {
        let mut pages = self.pages;
        for i in 0..Self::MAPPING.num_pts(2, self.pages) {
            let start = i * Self::MAPPING.span(2);
            let pt2 = self.pt2(start);
            for i2 in Self::MAPPING.range(2, start..self.pages) {
                let start = Self::MAPPING.page(2, start, i2);
                let pte2 = pt2.get(i2);

                pages -= if pte2.page() {
                    0
                } else {
                    let pt1 = self.pt1(start);
                    let mut free = 0;
                    for i1 in Self::MAPPING.range(1, start..self.pages) {
                        free += !pt1.get(i1) as usize;
                    }
                    assert_eq!(free, pte2.free(), "{pte2:?}: {pt1:?}");
                    free
                }
            }
        }
        pages
    }

    fn dbg_for_each_pte2<F: FnMut(Entry2)>(&self, mut f: F) {
        for i2 in 0..(self.pages / Self::MAPPING.span(2)) {
            let start = i2 * Self::MAPPING.span(2);
            let pt2 = self.pt2(start);
            for i1 in Self::MAPPING.range(2, start..self.pages) {
                f(pt2.get(i1));
            }
        }
    }
}

impl<const T2N: usize> CacheLower<T2N>
where
    [(); T2N / 2]:,
{
    /// Returns the l1 page table that contains the `page`.
    /// ```text
    /// NVRAM: [ Pages | padding | PT1s | PT2s | Meta ]
    /// ```
    fn pt1(&self, page: usize) -> &Bitfield {
        let mut offset = self.begin + self.pages * Page::SIZE;

        let i = page / Self::MAPPING.span(1);
        debug_assert!(i < Self::MAPPING.num_pts(1, self.pages));
        offset += i * Bitfield::SIZE;
        unsafe { &*(offset as *const Bitfield) }
    }

    /// Returns the l2 page table that contains the `page`.
    /// ```text
    /// NVRAM: [ Pages | padding | PT1s | PT2s | Meta ]
    /// ```
    fn pt2(&self, page: usize) -> &ATable<Entry2, T2N> {
        let mut offset = self.begin + self.pages * Page::SIZE;
        offset += Self::MAPPING.num_pts(1, self.pages) * Bitfield::SIZE;
        offset = align_up(offset, ATable::<Entry2, T2N>::SIZE); // correct alignment

        let i = page / Self::MAPPING.span(2);
        debug_assert!(i < Self::MAPPING.num_pts(2, self.pages));
        offset += i * ATable::<Entry2, T2N>::SIZE;
        unsafe { &*(offset as *mut ATable<Entry2, T2N>) }
    }

    /// Returns the l2 page table with pair entries that can be updated at once.
    fn pt2_pair(&self, page: usize) -> &ATable<Entry2Pair, { T2N / 2 }> {
        let pt2 = self.pt2(page);
        unsafe { &*((pt2 as *const ATable<Entry2, T2N>) as *const ATable<Entry2Pair, { T2N / 2 }>) }
    }

    fn recover_l1(&self, start: usize) -> usize {
        let pt = self.pt1(start);
        let mut pages = 0;
        for i in Self::MAPPING.range(1, start..self.pages) {
            pages += !pt.get(i) as usize;
        }
        pages
    }

    /// Allocate a single page
    fn get_small(&self, start: usize, order: usize) -> Result<usize> {
        let pt2 = self.pt2(start);

        for _ in 0..CAS_RETRIES {
            for newstart in Self::MAPPING.iterate(2, start) {
                let i2 = Self::MAPPING.idx(2, newstart);

                #[cfg(feature = "stop")]
                {
                    let pte2 = pt2.get(i2);
                    if pte2.page() || pte2.free() < 1 << order {
                        continue;
                    }
                    stop!();
                }

                if pt2.update(i2, |v| v.dec(1 << order)).is_ok() {
                    match self.get_table(newstart, order) {
                        // Revert conter
                        Err(Error::Memory) => {
                            if let Err(e) =
                                pt2.update(i2, |v| v.inc(Self::MAPPING.span(1), 1 << order))
                            {
                                error!("Corruption! {e:?}");
                                return Err(Error::Corruption);
                            }
                        }
                        ret => return ret,
                    }
                }
            }
        }
        info!("Nothing found o={order}");
        Err(Error::Memory)
    }

    /// Search free page table entry.
    fn get_table(&self, start: usize, order: usize) -> Result<usize> {
        let i = Self::MAPPING.idx(1, start);
        let pt1 = self.pt1(start);

        for _ in 0..CAS_RETRIES {
            if let Ok(i) = pt1.set_first_zeros(i, order) {
                return Ok(Self::MAPPING.page(1, start, i));
            }
            stop!();
        }
        info!("Nothing found o={order}");
        Err(Error::Memory)
    }

    /// Allocate huge page
    fn get_huge(&self, start: usize) -> Result<usize> {
        let pt2 = self.pt2(start);
        for _ in 0..CAS_RETRIES {
            for page in Self::MAPPING.iterate(2, start) {
                let i2 = Self::MAPPING.idx(2, page);
                if pt2
                    .update(i2, |v| v.mark_huge(Self::MAPPING.span(1)))
                    .is_ok()
                {
                    return Ok(Self::MAPPING.page(2, start, i2));
                }
            }
        }
        info!("Nothing found o=7..9");
        Err(Error::Memory)
    }

    /// Allocate multiple huge pages
    fn get_max(&self, start: usize) -> Result<usize> {
        let pt2_pair = self.pt2_pair(start);
        for _ in 0..CAS_RETRIES {
            for page in Self::MAPPING.iterate(2, start).step_by(2) {
                let i2 = Self::MAPPING.idx(2, page) / 2;
                if pt2_pair
                    .update(i2, |v| v.both(|v| v.mark_huge(Self::MAPPING.span(1))))
                    .is_ok()
                {
                    info!("Alloc o=10 i={i2}");
                    return Ok(Self::MAPPING.page(2, start, i2 * 2));
                }
            }
        }
        warn!("Nothing found o=10");
        Err(Error::Memory)
    }

    fn put_small(&self, page: usize, order: usize) -> Result<()> {
        stop!();

        let pt1 = self.pt1(page);
        let i1 = Self::MAPPING.idx(1, page);
        if pt1.toggle(i1, order, true).is_err() {
            error!("Invalid Addr l1 i{i1} p={page}");
            return Err(Error::Address);
        }

        stop!();

        let pt2 = self.pt2(page);
        let i2 = Self::MAPPING.idx(2, page);
        if let Err(pte2) = pt2.update(i2, |v| v.inc(Self::MAPPING.span(1), 1 << order)) {
            error!("Invalid Addr l1 i{i1} p={page} {pte2:?}");
            return Err(Error::Address);
        }

        Ok(())
    }

    pub fn put_max(&self, page: usize, order: usize) -> Result<()> {
        let pt2_pair = self.pt2_pair(page);
        let i2 = Self::MAPPING.idx(2, page) / 2;
        info!("Put o={order} i={i2}");
        if let Err(old) = pt2_pair.cas(
            i2,
            Entry2Pair(Entry2::new_page(), Entry2::new_page()),
            Entry2Pair(
                Entry2::new_free(Self::MAPPING.span(1)),
                Entry2::new_free(Self::MAPPING.span(1)),
            ),
        ) {
            error!("Addr {page:x} o={order} {old:?} i={i2}");
            Err(Error::Address)
        } else {
            Ok(())
        }
    }

    #[allow(dead_code)]
    pub fn dump(&self, start: usize) {
        let mut out = String::new();
        writeln!(out, "Dumping pt {}", start / Self::MAPPING.span(2)).unwrap();
        let pt2 = self.pt2(start);
        for i2 in 0..Self::MAPPING.len(2) {
            let start = Self::MAPPING.page(2, start, i2);
            if start > self.pages {
                return;
            }

            let pte2 = pt2.get(i2);
            let indent = (Self::MAPPING.levels() - 2) * 4;
            let pt1 = self.pt1(start);
            writeln!(out, "{:indent$}l2 i={i2}: {pte2:?}\t{pt1:?}", "").unwrap();
        }
        warn!("{out}");
    }
}

#[cfg(feature = "stop")]
#[cfg(all(test, feature = "std"))]
mod test {
    use std::sync::Arc;

    use alloc::vec::Vec;
    use log::warn;

    use super::CacheLower;
    use crate::lower::LowerAlloc;
    use crate::stop::{StopVec, Stopper};
    use crate::table::{Bitfield, Mapping};
    use crate::thread;
    use crate::util::{logging, Page, WyRand};

    const T2N: usize = 128;
    const MAPPING: Mapping<2> = CacheLower::<T2N>::MAPPING;

    fn count(pt: &Bitfield) -> usize {
        let mut pages = 0;
        for i in 0..Bitfield::LEN {
            pages += !pt.get(i) as usize;
        }
        pages
    }

    #[test]
    fn alloc_normal() {
        logging();

        let orders = [
            vec![0, 0, 1, 1],
            vec![0, 0, 1, 1, 1, 0, 0],
            vec![1, 1, 0, 0, 0, 1, 1],
            vec![1, 0, 1, 0, 0],
            vec![1, 1, 0, 0],
        ];

        let mut buffer = vec![Page::new(); 4 * MAPPING.span(2)];

        for order in orders {
            warn!("order: {order:?}");
            let lower = Arc::new(CacheLower::<T2N>::new(2, &mut buffer));
            lower.clear();
            lower.get(0, 0).unwrap();

            let stop = StopVec::new(2, order);

            let l = lower.clone();
            thread::parallel(2, move |t| {
                thread::pin(t);
                let key = Stopper::init(stop, t as _);

                let page = l.get(0, 0).unwrap();
                drop(key);
                assert!(page != 0);
            });

            assert_eq!(lower.pt2(0).get(0).free(), MAPPING.span(1) - 3);
            assert_eq!(count(lower.pt1(0)), MAPPING.span(1) - 3);
        }
    }

    #[test]
    fn alloc_first() {
        logging();

        let orders = [
            vec![0, 0, 1, 1],
            vec![0, 1, 1, 0, 0],
            vec![0, 1, 0, 1, 1],
            vec![1, 1, 0, 0],
        ];

        let mut buffer = vec![Page::new(); 4 * MAPPING.span(2)];

        for order in orders {
            warn!("order: {order:?}");
            let lower = Arc::new(CacheLower::<T2N>::new(2, &mut buffer));
            lower.clear();

            let stop = StopVec::new(2, order);
            let l = lower.clone();
            thread::parallel(2, move |t| {
                thread::pin(t);
                let _stopper = Stopper::init(stop, t as _);

                l.get(0, 0).unwrap();
            });

            let pte2 = lower.pt2(0).get(0);
            assert_eq!(pte2.free(), MAPPING.span(1) - 2);
            assert_eq!(count(lower.pt1(0)), MAPPING.span(1) - 2);
        }
    }

    #[test]
    fn alloc_last() {
        logging();

        let orders = [
            vec![0, 0, 1, 1, 1],
            vec![0, 1, 1, 0, 1, 1, 0],
            vec![1, 0, 0, 1, 0],
            vec![1, 1, 0, 0, 0],
        ];

        let mut buffer = vec![Page::new(); 4 * MAPPING.span(2)];

        for order in orders {
            warn!("order: {order:?}");
            let lower = Arc::new(CacheLower::<T2N>::new(2, &mut buffer));
            lower.clear();

            for _ in 0..MAPPING.span(1) - 1 {
                lower.get(0, 0).unwrap();
            }

            let stop = StopVec::new(2, order);
            let l = lower.clone();
            thread::parallel(2, move |t| {
                thread::pin(t);
                let _stopper = Stopper::init(stop, t as _);

                l.get(0, 0).unwrap();
            });

            let pt2 = lower.pt2(0);
            assert_eq!(pt2.get(0).free(), 0);
            assert_eq!(pt2.get(1).free(), MAPPING.span(1) - 1);
            assert_eq!(count(lower.pt1(MAPPING.span(1))), MAPPING.span(1) - 1);
        }
    }

    #[test]
    fn free_normal() {
        logging();

        let orders = [
            vec![0, 0, 0, 1, 1, 1], // first 0, then 1
            vec![0, 1, 0, 1, 0, 1, 0, 1],
            vec![0, 0, 1, 1, 1, 0, 0],
        ];

        let mut pages = [0; 2];
        let mut buffer = vec![Page::new(); 4 * MAPPING.span(2)];

        for order in orders {
            warn!("order: {order:?}");
            let lower = Arc::new(CacheLower::<T2N>::new(2, &mut buffer));
            lower.clear();

            pages[0] = lower.get(0, 0).unwrap();
            pages[1] = lower.get(0, 0).unwrap();

            let stop = StopVec::new(2, order);
            let l = lower.clone();
            thread::parallel(2, {
                let pages = pages.clone();
                move |t| {
                    let _stopper = Stopper::init(stop, t as _);

                    l.put(pages[t as usize], 0).unwrap();
                }
            });

            assert_eq!(lower.pt2(0).get(0).free(), MAPPING.span(1));
        }
    }

    #[test]
    fn free_last() {
        logging();

        let orders = [
            vec![0, 0, 0, 1, 1, 1],
            vec![0, 1, 0, 1, 0, 1, 0, 1],
            vec![0, 1, 1, 0, 0, 1, 1, 0],
        ];

        let mut pages = [0; MAPPING.span(1)];
        let mut buffer = vec![Page::new(); 4 * MAPPING.span(2)];

        for order in orders {
            warn!("order: {order:?}");
            let lower = Arc::new(CacheLower::<T2N>::new(2, &mut buffer));
            lower.clear();

            for page in &mut pages {
                *page = lower.get(0, 0).unwrap();
            }

            let stop = StopVec::new(2, order);
            let l = lower.clone();
            thread::parallel(2, move |t| {
                let _stopper = Stopper::init(stop, t as _);

                l.put(pages[t as usize], 0).unwrap();
            });

            let pt2 = lower.pt2(0);
            assert_eq!(pt2.get(0).free(), 2);
            assert_eq!(count(lower.pt1(0)), 2);
        }
    }

    #[test]
    fn realloc_last() {
        logging();

        let orders = [
            vec![0, 0, 0, 1, 1], // free then alloc
            vec![1, 1, 0, 0, 0], // alloc last then free last
            vec![0, 1, 1, 0, 0],
            vec![0, 0, 1, 1, 0],
            vec![1, 0, 1, 0, 0],
            vec![0, 1, 0, 1, 0],
            vec![0, 0, 1, 0, 1],
            vec![1, 0, 0, 0, 1],
        ];

        let mut pages = [0; MAPPING.span(1)];
        let mut buffer = vec![Page::new(); 4 * MAPPING.span(2)];

        for order in orders {
            warn!("order: {order:?}");
            let lower = Arc::new(CacheLower::<T2N>::new(2, &mut buffer));
            lower.clear();

            for page in &mut pages[..MAPPING.span(1) - 1] {
                *page = lower.get(0, 0).unwrap();
            }
            let stop = StopVec::new(2, order);

            let handle = std::thread::spawn({
                let stop = Arc::clone(&stop);
                let lower = lower.clone();
                move || {
                    let _stopper = Stopper::init(stop, 1);

                    lower.get(0, 0).unwrap();
                }
            });

            {
                let _stopper = Stopper::init(stop, 0);

                lower.put(pages[0], 0).unwrap();
            }

            handle.join().unwrap();

            let pt2 = lower.pt2(0);
            if pt2.get(0).free() == 1 {
                assert_eq!(count(lower.pt1(0)), 1);
            } else {
                // Table entry skipped
                assert_eq!(pt2.get(0).free(), 2);
                assert_eq!(count(lower.pt1(0)), 2);
                assert_eq!(pt2.get(1).free(), MAPPING.span(1) - 1);
                assert_eq!(count(lower.pt1(MAPPING.span(1))), MAPPING.span(1) - 1);
            }
        }
    }

    #[test]
    fn alloc_normal_large() {
        logging();

        let orders = [
            vec![0, 0, 1, 1],
            vec![0, 0, 1, 1, 1, 0, 0],
            vec![1, 1, 0, 0, 0, 1, 1],
            vec![1, 0, 1, 0, 0],
            vec![1, 1, 0, 0],
        ];

        let mut buffer = vec![Page::new(); 4 * MAPPING.span(2)];

        for order in orders {
            warn!("order: {order:?}");
            let lower = Arc::new(CacheLower::<T2N>::new(2, &mut buffer));
            lower.clear();
            lower.get(0, 0).unwrap();

            let stop = StopVec::new(2, order);

            let l = lower.clone();
            thread::parallel(2, move |t| {
                thread::pin(t);
                let key = Stopper::init(stop, t as _);

                let order = t + 1; // order 1 and 2
                let page = l.get(0, order).unwrap();
                drop(key);
                assert!(page != 0);
            });

            let allocated = 1 + 2 + 4;
            assert_eq!(lower.pt2(0).get(0).free(), MAPPING.span(1) - allocated);
            assert_eq!(count(lower.pt1(0)), MAPPING.span(1) - allocated);
        }
    }

    #[test]
    fn free_normal_large() {
        logging();

        let orders = [
            vec![0, 0, 0, 1, 1, 1], // first 0, then 1
            vec![0, 1, 0, 1, 0, 1, 0, 1],
            vec![0, 0, 1, 1, 1, 0, 0],
        ];

        let mut pages = [0; 2];
        let mut buffer = vec![Page::new(); 4 * MAPPING.span(2)];

        for order in orders {
            warn!("order: {order:?}");
            let lower = Arc::new(CacheLower::<T2N>::new(2, &mut buffer));
            lower.clear();

            pages[0] = lower.get(0, 1).unwrap();
            pages[1] = lower.get(0, 2).unwrap();

            assert_eq!(lower.pt2(0).get(0).free(), MAPPING.span(1) - 2 - 4);

            let stop = StopVec::new(2, order);
            let l = lower.clone();
            thread::parallel(2, {
                let pages = pages.clone();
                move |t| {
                    let _stopper = Stopper::init(stop, t as _);

                    l.put(pages[t as usize], t + 1).unwrap();
                }
            });

            assert_eq!(lower.pt2(0).get(0).free(), MAPPING.span(1));
        }
    }

    #[test]
    fn different_orders() {
        logging();

        const MAX_ORDER: usize = CacheLower::<T2N>::MAX_ORDER;
        const HUGE_ORDER: usize = CacheLower::<T2N>::HUGE_ORDER;
        const MAX_LARGE_ORDER: usize = 6;
        let mut buffer = vec![Page::new(); MAPPING.span(2)];

        thread::pin(0);
        let lower = Arc::new(CacheLower::<T2N>::new(1, &mut buffer));
        lower.clear();

        assert_eq!(lower.dbg_allocated_pages(), 0);

        let mut rng = WyRand::new(42);

        let mut num_pages = 0;
        let mut pages = Vec::new();
        for order in 0..=MAX_ORDER {
            for _ in 0..1 << (MAX_ORDER - order) {
                pages.push((order, 0));
                num_pages += if order <= MAX_LARGE_ORDER {
                    1 << order
                } else {
                    MAPPING.span(1) * ((order - 1) / HUGE_ORDER + 1)
                };
            }
        }
        rng.shuffle(&mut pages);
        warn!("allocate {num_pages} pages up to order {MAX_ORDER}");

        for (order, page) in &mut pages {
            *page = lower.get(0, *order).unwrap();
        }

        lower.dump(0);
        assert_eq!(lower.dbg_allocated_pages(), num_pages);

        for (order, page) in &pages {
            lower.put(*page, *order).unwrap();
        }

        assert_eq!(lower.dbg_allocated_pages(), 0);
    }
}
