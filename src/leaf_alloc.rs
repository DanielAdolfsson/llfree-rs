use std::sync::atomic::{AtomicUsize, Ordering};

use log::{error, info, warn};

use crate::alloc::alloc;
use crate::entry::{Entry1, Entry2};
use crate::table::{self, Table, LAYERS, PAGE_SIZE, PT_LEN, PT_LEN_BITS};

use crate::{Error, Result, Size};

#[cfg(test)]
macro_rules! wait {
    () => {
        if let Err(e) = crate::wait::wait() {
            error!("{:?}", e);
            panic!("{:?}", e);
        }
    };
}
#[cfg(not(test))]
macro_rules! wait {
    () => {};
}

/// Layer 2 page allocator.
#[repr(align(64))]
pub struct LeafAllocator {
    pub begin: usize,
    pub pages: usize,
    alloc_pt1: AtomicUsize,
    pub small_start: usize,
    pub huge_start: usize,
}

impl Clone for LeafAllocator {
    fn clone(&self) -> Self {
        Self {
            begin: self.begin,
            pages: self.pages,
            alloc_pt1: AtomicUsize::new(0),
            small_start: self.small_start,
            huge_start: self.huge_start,
        }
    }
}

impl LeafAllocator {
    pub const fn new(begin: usize, pages: usize) -> Self {
        Self {
            begin,
            pages,
            alloc_pt1: AtomicUsize::new(0),
            small_start: 0,
            huge_start: 0,
        }
    }

    pub fn clear(&self) {
        // Init pt2
        for i in 0..table::num_pts(2, self.pages) {
            let pt2 = self.pt2(i * table::span(2));
            pt2.clear();
        }
        // Init pt1
        for i in 0..table::num_pts(1, self.pages) {
            let pt1 = unsafe { &*((self.begin + i * table::m_span(1)) as *const Table<Entry1>) };
            pt1.clear();
        }
    }

    /// Returns the l1 page table that contains the `page`.
    fn pt1(&self, pte2: Entry2, page: usize) -> &Table<Entry1> {
        let page = table::page(1, page, pte2.i1());
        unsafe { &*((self.begin + page * PAGE_SIZE) as *const Table<Entry1>) }
    }

    /// Returns the l2 page table that contains the `page`.
    /// ```text
    /// NVRAM: [ Pages & PT1 | PT2 | Meta ]
    /// ```
    fn pt2(&self, page: usize) -> &Table<Entry2> {
        let i = page >> (PT_LEN_BITS * 2);
        unsafe { &*((self.begin + (self.pages + i) * PAGE_SIZE) as *mut Table<Entry2>) }
    }

    pub fn recover(&self, start: usize, deep: bool) -> (usize, Size) {
        let mut pages = 0;
        let mut size = Size::L0;

        let pt = self.pt2(start);
        for i in table::range(2, start..self.pages) {
            let pte = pt.get(i);
            if pte.giant() {
                return (table::span(2), Size::L2);
            } else if pte.page() {
                size = Size::L1;
                pages += table::span(1);
            } else if deep {
                let p = self.recover_l1(table::page(1, start, i), pte);
                if pte.pages() != p {
                    warn!(
                        "Invalid PTE2 start=0x{:x} i{}: {} != %{}",
                        start,
                        i,
                        pte.pages(),
                        p
                    );
                    pt.set(i, pte.with_pages(p));
                }
                pages += p;
            } else {
                pages += pte.pages();
            }
        }

        (pages, size)
    }

    fn recover_l1(&self, start: usize, pte2: Entry2) -> usize {
        let pt = self.pt1(pte2, start);
        let mut pages = 0;
        for i in table::range(1, start..self.pages) {
            if pt.get(i) == Entry1::Page {
                pages += 1;
            }
        }
        return pages;
    }

    /// Allocate a single page
    pub fn get(&self, start: usize) -> usize {
        let pt2 = self.pt2(start);

        loop {
            for i2 in table::range(2, start..self.pages) {
                let newstart = table::page(2, start, i2);

                wait!();

                let pte2 = pt2.get(i2);

                if pte2.page() || pte2.pages() as usize >= PT_LEN {
                    continue;
                }

                self.alloc_pt1
                    .store(!table::page(1, start, pte2.i1()), Ordering::SeqCst);

                wait!();

                if let Err(_) = pt2.update(i2, |pte| pte.inc(pte2.i1())) {
                    self.alloc_pt1.store(0, Ordering::SeqCst);
                    continue;
                }

                if pte2.pages() == PT_LEN - 1 {
                    return self.get_last(pte2, newstart);
                }
                return self.get_table(pte2, newstart);
            }
        }
    }

    /// Search free page table entry.
    fn get_table(&self, pte2: Entry2, start: usize) -> usize {
        let pt1 = self.pt1(pte2, start);

        loop {
            for i in table::range(1, start..self.pages) {
                if i == pte2.i1() {
                    continue;
                }

                if pt1.get(i) != Entry1::Empty {
                    continue;
                }

                wait!();

                if pt1.cas(i, Entry1::Empty, Entry1::Page).is_ok() {
                    let page = table::page(1, start, i);
                    info!("alloc l1 i={}: {}", i, page);

                    self.alloc_pt1.store(0, Ordering::SeqCst);
                    return page;
                }
            }

            warn!("nothing found retry...");
            wait!();
        }
    }

    /// Allocate the last page (the pt1 is reused as last page).
    fn get_last(&self, pte2: Entry2, start: usize) -> usize {
        wait!();
        info!("alloc last {} s={}", pte2.i1(), start);

        let pt1 = self.pt1(pte2, start);
        let i1 = pte2.i1();
        let alloc_p1 = !table::page(1, start, i1);

        // Wait for others to finish
        for (i, leaf) in alloc().local.iter().enumerate() {
            if leaf as *const _ != self as *const _ {
                while leaf.alloc_pt1.load(Ordering::SeqCst) == alloc_p1 {
                    warn!("Waiting for cpu {} on {}", i, unsafe {
                        (self as *const Self).offset_from(&alloc().local[0] as *const _)
                    });
                    wait!();
                }
            }
        }

        if pt1.cas(i1, Entry1::Empty, Entry1::Page).is_err() {
            panic!("Corruption l1 i{}", i1);
        }

        self.alloc_pt1.store(0, Ordering::SeqCst);
        start + pte2.i1()
    }

    pub fn get_huge(&self, start: usize) -> usize {
        let pt = self.pt2(start);
        loop {
            for i in table::range(2, start..self.pages) {
                if pt
                    .cas(i, Entry2::new(), Entry2::new().with_page(true))
                    .is_ok()
                {
                    info!("alloc l2 i={}: {}", i, table::page(2, start, i));
                    return table::page(2, start, i);
                }
            }
        }
    }

    /// Free single page
    pub fn put(&self, page: usize) -> Result<Size> {
        let pt2 = self.pt2(page);
        let i2 = table::idx(2, page);

        wait!();

        let old = pt2.get(i2);

        if old.page() {
            // Free huge page
            if page % table::span(Size::L1 as _) != 0 {
                error!("Invalid address {}", page);
                return Err(Error::Address);
            }

            let pt1 = unsafe { &*((self.begin + page * PAGE_SIZE) as *const Table<Entry1>) };
            pt1.clear();

            match pt2.cas(i2, old, Entry2::new()) {
                Ok(_) => Ok(Size::L1),
                Err(_) => panic!("Corruption l2 i{}", i2),
            }
        } else if !old.giant() && old.pages() > 0 {
            self.put_small(old, page).map(|_| Size::L0)
        } else {
            Err(Error::Address)
        }
    }

    fn put_small(&self, pte2: Entry2, page: usize) -> Result<()> {
        info!("free leaf page {}", page);
        let pt2 = self.pt2(page);
        let i2 = table::idx(2, page);

        if pte2.pages() == PT_LEN {
            return self.put_full(page);
        }

        let pt1 = self.pt1(pte2, page);
        let i1 = table::idx(1, page);
        let pte1 = pt1.get(i1);

        if pte1 != Entry1::Page {
            error!("Invalid Addr l1 i{} p={}", i1, page);
            return Err(Error::Address);
        }

        wait!();

        if let Err(pte2) = pt2.update(i2, |pte| pte.dec(pte2.i1())) {
            return if pte2.pages() == 0 {
                error!("Invalid Addr l1 i{} p={}", i1, page);
                Err(Error::Address)
            } else {
                Err(Error::CAS)
            };
        }

        if pt1.cas(i1, Entry1::Page, Entry1::Empty).is_err() {
            panic!("Corruption l1 i{}", i1);
        }

        Ok(())
    }

    /// Free last page & rebuild pt1 in it
    fn put_full(&self, page: usize) -> Result<()> {
        let pt2 = self.pt2(page);
        let i2 = table::idx(2, page);

        wait!();

        // The freed page becomes the new pt
        let pt1 = unsafe { &*((self.begin + page * PAGE_SIZE) as *const Table<Entry1>) };
        info!("free: init last pt1 {}", page);

        for j in 0..PT_LEN {
            if j == page % PT_LEN {
                pt1.set(j, Entry1::Reserved);
            } else {
                pt1.set(j, Entry1::Page);
            }
        }

        match pt2.cas(
            i2,
            Entry2::new_table(PT_LEN, 0),
            Entry2::new_table(PT_LEN - 1, page % PT_LEN),
        ) {
            Ok(_) => Ok(()),
            Err(pte) => {
                warn!("CAS: create pt1 {:?}", pte);
                Err(Error::CAS)
            }
        }
    }

    pub fn clear_giant(&self, page: usize) {
        for j in table::range(2, page..self.pages) {
            // i1 is initially 0
            let pt1 = unsafe {
                &*((self.begin + (page + j * table::span(1)) * PAGE_SIZE) as *const Table<Entry1>)
            };
            pt1.clear();
        }
        // Clear the persist flag
        self.pt2(page).set(0, Entry2::new());
    }

    pub fn dump(&self, start: usize) {
        let pt2 = self.pt2(start);
        for i2 in 0..PT_LEN {
            let start = start + i2 * PT_LEN;
            if start >= self.pages {
                return;
            }

            let pte2 = pt2.get(i2);
            info!(
                "{:1$}l2 i={2} 0x{3:x}: {4:?}",
                "",
                (LAYERS - 2) * 4,
                i2,
                start * PAGE_SIZE,
                pte2
            );
            if !pte2.giant() && !pte2.page() && pte2.pages() > 0 && pte2.pages() < PT_LEN {
                let pt1 = self.pt1(pte2, start);
                for i1 in 0..PT_LEN {
                    let pte1 = pt1.get(i1);
                    info!(
                        "{:1$}l1 i={2} 0x{3:x}: {4:?}",
                        "",
                        (LAYERS - 1) * 4,
                        i1,
                        (start + i1) * PAGE_SIZE,
                        pte1
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::alloc::{alloc_zeroed, Layout};
    use std::sync::Arc;
    use std::thread;

    use log::warn;

    use crate::alloc::alloc;
    use crate::entry::Entry1;
    use crate::table::{Table, PAGE_SIZE, PT_LEN};
    use crate::util::{logging, parallel};
    use crate::wait::{DbgWait, DbgWaitKey};
    use crate::{cpu, table};

    use super::LeafAllocator;

    fn aligned_buffer(size: usize) -> Vec<u8> {
        let buffer = unsafe {
            Vec::from_raw_parts(
                alloc_zeroed(Layout::from_size_align_unchecked(size, PAGE_SIZE)),
                size,
                size,
            )
        };
        assert!(buffer.as_ptr() as usize % PAGE_SIZE == 0);
        buffer
    }

    fn count(pt: &Table<Entry1>) -> usize {
        let mut pages = 0;
        for i in 0..PT_LEN {
            pages += (pt.get(i) == Entry1::Page) as usize;
        }
        pages
    }

    #[test]
    fn alloc_normal() {
        logging();
        let buffer = aligned_buffer(4 * table::m_span(2));
        alloc().init(2, buffer.as_ptr() as _, buffer.len()).unwrap();
        // init
        {
            let page = alloc().leaf_alloc(0).get(0);
            warn!("setup single alloc {}", page);
        }
        alloc().uninit();

        let orders = [
            vec![0, 0, 0, 1, 1, 1],
            vec![0, 0, 1, 1, 1, 0, 0],
            vec![1, 1, 0, 0, 0, 1, 1],
            vec![1, 0, 1, 0, 1, 0, 0],
        ];

        for order in orders {
            warn!("order: {:?}", order);
            let copy = buffer.clone();
            alloc().init(2, copy.as_ptr() as _, copy.len()).unwrap();

            let wait = DbgWait::setup(2, order);

            parallel(2, move |t| {
                cpu::pin(t);
                let key = DbgWaitKey::init(wait, t as _);
                let page_alloc = alloc().leaf_alloc(t);

                let page = page_alloc.get(0);
                drop(key);
                assert!(page != 0);
            });

            let page_alloc = alloc().leaf_alloc(0);
            assert_eq!(page_alloc.pt2(0).get(0).pages(), 3);
            assert_eq!(count(page_alloc.pt1(page_alloc.pt2(0).get(0), 0)), 3);
        }
    }

    #[test]
    fn alloc_first() {
        logging();
        const MEM_SIZE: usize = (PT_LEN + 1) * PAGE_SIZE;
        let buffer = aligned_buffer(MEM_SIZE);

        let orders = [
            vec![0, 0, 0, 1, 1, 1],
            vec![0, 1, 1, 0, 0, 1, 1],
            vec![0, 1, 0, 1, 0, 1, 1],
            vec![0, 1, 1, 1, 0, 0],
        ];

        for order in orders {
            warn!("order: {:?}", order);
            let copy = buffer.clone();
            let begin = copy.as_ptr() as usize;
            let wait = DbgWait::setup(2, order);

            parallel(2, move |t| {
                cpu::pin(t);
                let _key = DbgWaitKey::init(wait, t as _);
                let page_alloc = LeafAllocator::new(begin, PT_LEN);

                page_alloc.get(0);
            });

            let page_alloc = LeafAllocator::new(begin, PT_LEN);
            assert_eq!(page_alloc.pt2(0).get(0).pages(), 2);
            assert_eq!(count(page_alloc.pt1(page_alloc.pt2(0).get(0), 0)), 2);
        }
    }

    #[test]
    fn alloc_last() {
        logging();
        const MEM_SIZE: usize = 2 * (PT_LEN + 1) * PAGE_SIZE;
        let buffer = aligned_buffer(MEM_SIZE);

        let orders = [
            vec![0, 0, 0, 1, 1, 1, 1],
            vec![0, 0, 1, 1, 0, 1, 1, 0], // wait for other cpu
            vec![1, 0, 0, 1, 1, 1, 1, 0],
            vec![1, 1, 0, 1, 0, 0, 0],
        ];

        // init
        {
            let page_alloc = LeafAllocator::new(buffer.as_ptr() as _, 2 * PT_LEN);
            for _ in 0..PT_LEN - 1 {
                page_alloc.get(0);
            }
            warn!("setup single alloc");
        }

        for order in orders {
            warn!("order: {:?}", order);
            let copy = buffer.clone();
            let begin = copy.as_ptr() as usize;
            let wait = DbgWait::setup(2, order);

            parallel(2, move |t| {
                cpu::pin(t);
                let _key = DbgWaitKey::init(wait, t as _);
                let page_alloc = LeafAllocator::new(begin, 2 * PT_LEN);

                page_alloc.get(0);
            });

            let page_alloc = LeafAllocator::new(begin, 2 * PT_LEN);
            let pt2 = page_alloc.pt2(0);
            assert_eq!(pt2.get(0).pages(), PT_LEN);
            assert_eq!(pt2.get(1).pages(), 1);
            assert_eq!(count(page_alloc.pt1(pt2.get(1), PT_LEN)), 1);
        }
    }

    #[test]
    fn free_normal() {
        logging();
        const MEM_SIZE: usize = (PT_LEN + 1) * PAGE_SIZE;
        let buffer = aligned_buffer(MEM_SIZE);

        let orders = [
            vec![0, 0, 0, 1, 1, 1], // first 0, then 1
            vec![0, 1, 0, 1, 0, 1],
            vec![0, 0, 1, 1, 1, 0],
        ];

        let mut pages = [0; 2];

        // init
        {
            let page_alloc = LeafAllocator::new(buffer.as_ptr() as _, PT_LEN);
            pages[0] = page_alloc.get(0);
            pages[1] = page_alloc.get(0);
            warn!("setup single alloc");
        }

        for order in orders {
            warn!("order: {:?}", order);
            let copy = buffer.clone();
            let begin = copy.as_ptr() as usize;
            let wait = DbgWait::setup(2, order);

            parallel(2, {
                let pages = pages.clone();
                move |t| {
                    let _key = DbgWaitKey::init(wait, t as _);
                    let page_alloc = LeafAllocator::new(begin, PT_LEN);

                    match page_alloc.put(pages[t as usize]) {
                        Err(crate::Error::CAS) => {
                            page_alloc.put(pages[t as usize]).unwrap();
                        }
                        Err(e) => panic!("{:?}", e),
                        Ok(_) => {}
                    }
                }
            });

            let page_alloc = LeafAllocator::new(begin, PT_LEN);
            assert_eq!(page_alloc.pt2(0).get(0).pages(), 0);
        }
    }

    #[test]
    fn free_last() {
        logging();
        const MEM_SIZE: usize = (PT_LEN + 1) * PAGE_SIZE;
        let buffer = aligned_buffer(MEM_SIZE);

        let orders = [
            vec![0, 0, 1, 1, 1],       // first 0, then 1
            vec![0, 1, 0, 1, 1, 1, 1], // 1 fails cas
            vec![0, 1, 1, 0, 0, 0, 0], // 0 fails cas
        ];

        let mut pages = [0; PT_LEN];

        // init
        {
            let page_alloc = LeafAllocator::new(buffer.as_ptr() as _, PT_LEN);
            for page in &mut pages {
                *page = page_alloc.get(0);
            }
            warn!("setup single alloc");
        }

        for order in orders {
            warn!("order: {:?}", order);
            let buffer = buffer.clone();
            let begin = buffer.as_ptr() as usize;
            let wait = DbgWait::setup(2, order);

            parallel(2, {
                let pages = pages.clone();
                move |t| {
                    let _key = DbgWaitKey::init(wait, t as _);
                    let page_alloc = LeafAllocator::new(begin, PT_LEN);

                    match page_alloc.put(pages[t as usize]) {
                        Err(crate::Error::CAS) => {
                            page_alloc.put(pages[t as usize]).unwrap();
                        }
                        Err(e) => panic!("{:?}", e),
                        Ok(_) => {}
                    }
                }
            });

            let page_alloc = LeafAllocator::new(begin, PT_LEN);
            let pt2 = page_alloc.pt2(0);
            assert_eq!(pt2.get(0).pages(), PT_LEN - 2);
            assert_eq!(count(page_alloc.pt1(pt2.get(0), 0)), PT_LEN - 2);
        }
    }

    #[test]
    fn alloc_free_last() {
        logging();

        const MEM_SIZE: usize = 2 * (PT_LEN + 1) * PAGE_SIZE;
        let buffer = aligned_buffer(MEM_SIZE);

        let orders = [
            vec![0, 0, 0, 1, 1, 1, 1],       // 0 free then 1 alloc
            vec![1, 1, 0, 0],                // 1 alloc last then 0 free last
            vec![0, 1, 1, 1, 1, 0, 0],       // 1 skips table
            vec![1, 0, 1, 0, 0, 1, 1, 1, 1], // 1 fails cas
        ];

        let mut pages = [0; PT_LEN];

        {
            let page_alloc = LeafAllocator::new(buffer.as_ptr() as _, 2 * PT_LEN);
            for page in &mut pages[..PT_LEN - 1] {
                *page = page_alloc.get(0);
            }
            warn!("setup single alloc");
        }

        for order in orders {
            warn!("order: {:?}", order);
            let buffer = buffer.clone();
            let begin = buffer.as_ptr() as usize;
            let wait = DbgWait::setup(2, order);

            let wait_clone = Arc::clone(&wait);
            let handle = thread::spawn(move || {
                let _key = DbgWaitKey::init(wait_clone, 1);
                let page_alloc = LeafAllocator::new(begin, 2 * PT_LEN);
                page_alloc.get(0);
            });

            {
                let _key = DbgWaitKey::init(wait, 0);
                let page_alloc = LeafAllocator::new(begin, 2 * PT_LEN);

                match page_alloc.put(pages[0]) {
                    Err(crate::Error::CAS) => {
                        page_alloc.put(pages[0]).unwrap();
                    }
                    Err(e) => panic!("{:?}", e),
                    Ok(_) => {}
                }
            }

            handle.join().unwrap();

            let page_alloc = LeafAllocator::new(begin, 2 * PT_LEN);
            let pt2 = page_alloc.pt2(0);
            if pt2.get(0).pages() == PT_LEN - 1 {
                assert_eq!(count(page_alloc.pt1(pt2.get(0), 0)), PT_LEN - 1);
            } else {
                // Table entry skipped
                assert_eq!(pt2.get(0).pages(), PT_LEN - 2);
                assert_eq!(count(page_alloc.pt1(pt2.get(0), 0)), PT_LEN - 2);
                assert_eq!(pt2.get(1).pages(), 1);
                assert_eq!(count(page_alloc.pt1(pt2.get(1), PT_LEN)), 1);
            }
        }
    }
}