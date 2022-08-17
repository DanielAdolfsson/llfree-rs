use std::slice;
use std::time::Instant;

use clap::Parser;
use log::warn;
use nvalloc::mmap::MMap;
use nvalloc::table::PT_LEN;
use nvalloc::util::{div_ceil, Page, logging};

/// Crash testing an allocator.
#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    /// Number of threads
    #[clap(short, long, default_value = "6")]
    threads: usize,
    /// Max amount of memory in GiB. Is by the max thread count.
    #[clap(short, long, default_value_t = 16)]
    memory: usize,
    /// DAX file to be used for the allocator.
    #[clap(long)]
    dax: Option<String>,
}

fn main() {
    let Args {
        threads,
        memory,
        dax,
    } = Args::parse();

    logging();

    assert!(threads > 0 && memory > 0);

    let mut mapping = mapping(0x1000_0000_0000, memory * PT_LEN * PT_LEN, dax).unwrap();
    let pages = mapping.len();

    let timer = Instant::now();
    let handles = mapping
        .chunks_mut(div_ceil(pages, threads))
        .map(|chunk| {
            let start = chunk.as_ptr() as usize;
            let len = chunk.len();
            std::thread::spawn(move || {
                let chunk = unsafe { slice::from_raw_parts_mut(start as *mut Page, len) };
                let timer = Instant::now();
                for page in chunk {
                    *page.cast_mut::<usize>() = 1;
                }
                timer.elapsed().as_millis()
            })
        })
        .collect::<Vec<_>>();

    let mut time = 0;
    for t in handles {
        time += t.join().unwrap();
    }
    time /= threads as u128;

    warn!("avg: {time}");
    warn!("total: {}", timer.elapsed().as_millis());
}

#[allow(unused_variables)]
fn mapping(
    begin: usize,
    length: usize,
    dax: Option<String>,
) -> core::result::Result<MMap<Page>, ()> {
    #[cfg(target_os = "linux")]
    if length > 0 {
        if let Some(file) = dax {
            warn!(
                "MMap file {file} l={}G ({:x})",
                (length * std::mem::size_of::<Page>()) >> 30,
                length * std::mem::size_of::<Page>()
            );
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(file)
                .unwrap();
            return MMap::dax(begin, length, f);
        }
    }
    MMap::anon(begin, length)
}