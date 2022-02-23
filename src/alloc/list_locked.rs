use std::ops::Range;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use log::{error, warn};
use spin::mutex::TicketMutex;

use super::{Alloc, Error, Result, Size, MIN_PAGES};
use crate::util::Page;

#[repr(align(64))]
pub struct ListLockedAlloc {
    memory: Range<*const Page>,
    next: TicketMutex<Node>,
    local: Vec<LocalCounter>,
}

#[repr(align(64))]
struct LocalCounter {
    counter: AtomicUsize,
}

unsafe impl Send for ListLockedAlloc {}
unsafe impl Sync for ListLockedAlloc {}

impl ListLockedAlloc {
    pub fn new() -> Self {
        Self {
            memory: null()..null(),
            next: TicketMutex::new(Node(AtomicPtr::new(null_mut()))),
            local: Vec::new(),
        }
    }
}

impl Alloc for ListLockedAlloc {
    #[cold]
    fn init(&mut self, cores: usize, memory: &mut [Page], _overwrite: bool) -> Result<()> {
        warn!(
            "initializing c={cores} {:?} {}",
            memory.as_ptr_range(),
            memory.len()
        );
        if memory.len() < cores * MIN_PAGES {
            error!("Not enough memory {} < {}", memory.len(), cores * MIN_PAGES);
            return Err(Error::Memory);
        }

        let begin = memory.as_ptr() as usize;
        let pages = memory.len();

        // build free lists
        for i in 1..pages {
            memory[i - 1]
                .cast_mut::<Node>()
                .set((begin + i * Page::SIZE) as *mut _);
        }
        memory[pages - 1].cast_mut::<Node>().set(null_mut());

        self.memory = memory.as_ptr_range();
        self.next = TicketMutex::new(Node(AtomicPtr::new(begin as _)));

        self.local = Vec::with_capacity(cores);
        self.local.resize_with(cores, || LocalCounter {
            counter: AtomicUsize::new(0),
        });

        Ok(())
    }

    #[cold]
    fn destroy(&mut self) {}

    fn get(&self, core: usize, size: Size) -> Result<u64> {
        if size != Size::L0 {
            error!("{size:?} not supported");
            return Err(Error::Memory);
        }

        if let Some(node) = self.next.lock().pop() {
            self.local[core].counter.fetch_add(1, Ordering::Relaxed);
            let addr = node as *mut _ as u64;
            debug_assert!(addr % Page::SIZE as u64 == 0 && self.memory.contains(&(addr as _)),);
            Ok(addr)
        } else {
            error!("No memory");
            Err(Error::Memory)
        }
    }

    fn put(&self, core: usize, addr: u64) -> Result<()> {
        if addr % Page::SIZE as u64 != 0 || !self.memory.contains(&(addr as _)) {
            error!("invalid addr");
            return Err(Error::Address);
        }

        self.next.lock().push(unsafe { &mut *(addr as *mut Node) });
        self.local[core].counter.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }

    #[cold]
    fn allocated_pages(&self) -> usize {
        self.local
            .iter()
            .map(|c| c.counter.load(Ordering::SeqCst))
            .sum()
    }
}

struct Node(AtomicPtr<Node>);

impl Node {
    fn set(&self, v: *mut Node) {
        self.0.store(v, Ordering::Relaxed);
    }
    fn push(&self, v: &mut Node) {
        let next = self.0.load(Ordering::Relaxed);
        v.0.store(next, Ordering::Relaxed);
        self.0.store(v, Ordering::Relaxed);
    }
    fn pop(&self) -> Option<&mut Node> {
        let curr = self.0.load(Ordering::Relaxed);
        if !curr.is_null() {
            let curr = unsafe { &mut *curr };
            let next = curr.0.load(Ordering::Relaxed);
            self.0.store(next, Ordering::Relaxed);
            Some(curr)
        } else {
            None
        }
    }
}
