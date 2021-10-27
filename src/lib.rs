//! Simple reduced alloc example.

use std::{cell::RefCell, sync::atomic::AtomicU64};

mod alloc;
mod mmap;
mod paging;
mod util;

use alloc::{Allocator, ChunkSize, Error};

thread_local! {
    static ALLOC: RefCell<Option<Allocator>> = RefCell::new(None);
}

pub fn init(addr: *mut (), size: usize) -> alloc::Result<()> {
    ALLOC.with(|a| {
        *a.borrow_mut() = Some(Allocator::init(addr as usize, size)?);
        Ok(())
    })?;

    Ok(())
}

pub fn get<F: FnOnce(u64) -> u64>(
    size: ChunkSize,
    dst: &AtomicU64,
    translate: F,
    expected: u64,
) -> alloc::Result<()> {
    ALLOC.with(|a| {
        let mut a = a.borrow_mut();

        if let Some(a) = a.as_mut() {
            a.get(size, dst, translate, expected)
        } else {
            Err(Error::Uninitialized)
        }
    })?;

    Ok(())
}

pub fn put(addr: u64, size: ChunkSize) -> alloc::Result<()> {
    ALLOC.with(|a| {
        let mut a = a.borrow_mut();

        if let Some(a) = a.as_mut() {
            a.put(addr, size)
        } else {
            Err(Error::Uninitialized)
        }
    })?;

    Ok(())
}

#[cfg(test)]
pub(crate) fn logging() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .init();
}

#[cfg(test)]
mod test {
    use std::slice;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;

    use log::info;

    use crate::alloc::{ChunkSize, MAX_SIZE};
    use crate::mmap::c_mmap_anon;
    use crate::paging::PT_LEN;
    use crate::{get, init, logging, put};

    #[test]
    fn threading() {
        logging();

        let data = unsafe { slice::from_raw_parts(0x1000_0000_0000_u64 as _, MAX_SIZE) };

        info!("mmap {} bytes", data.len());

        c_mmap_anon(data).unwrap();

        info!("init alloc");

        let addr = data.as_ptr() as _;
        let size = data.len();

        init(addr, size).unwrap();

        info!("init finished");
        const DEFAULT: AtomicU64 = AtomicU64::new(0);

        let addr = addr as usize;
        let threads = (0..10)
            .into_iter()
            .map(|_| {
                thread::spawn(move || {
                    init(addr as _, size).unwrap();

                    let pages = [DEFAULT; PT_LEN];
                    for page in &pages {
                        get(ChunkSize::Page, page, |v| v, 0).unwrap();
                    }

                    for i in 0..pages.len() {
                        for j in i + 1..pages.len() {
                            assert_ne!(
                                pages[i].load(Ordering::Acquire),
                                pages[j].load(Ordering::Acquire),
                                "{}=={}",
                                i,
                                j
                            );
                        }
                    }

                    for page in &pages {
                        put(page.load(Ordering::Acquire), ChunkSize::Page).unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();

        thread::sleep(Duration::from_secs(1));

        for t in threads {
            t.join().unwrap();
        }

        info!("Finish");
        super::ALLOC.with(|a| {
            let mut a = a.borrow_mut();
            if let Some(a) = a.as_mut() {
                a.dump();
            }
        });
    }
}
