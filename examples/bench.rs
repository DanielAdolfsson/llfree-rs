#![cfg(all(feature = "thread", feature = "logger"))]

use core::fmt;
use std::any::type_name;
use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Barrier};
use std::time::Instant;

use clap::Parser;
use log::warn;

use nvalloc::alloc::buddy::BuddyAlloc;
use nvalloc::alloc::local_lists::LocalListAlloc;
use nvalloc::alloc::malloc::MallocAlloc;
use nvalloc::alloc::stack::StackAlloc;
use nvalloc::alloc::table::TableAlloc;
use nvalloc::alloc::{Alloc, Error, Size, MIN_PAGES};
use nvalloc::mmap::MMap;
use nvalloc::table::Table;
use nvalloc::util::{Cycles, Page};
use nvalloc::{thread, util};

/// Benchmarking the allocators against each other.
#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    #[clap(long, default_value_t = 1)]
    min_threads: usize,
    #[clap(short = 't', long, default_value_t = 4)]
    max_threads: usize,
    #[clap(short, long, default_value = "bench/bench.csv")]
    outfile: String,
    #[clap(long)]
    dax: Option<String>,
    #[clap(short, long, default_value_t = 1)]
    iterations: usize,
    #[clap(short, long, default_value_t = 0)]
    size: usize,
    #[clap(long, default_value_t = 1)]
    cpu_stride: usize,
}

fn main() {
    let Args {
        min_threads,
        max_threads,
        outfile,
        dax,
        iterations,
        size,
        cpu_stride,
    } = Args::parse();

    util::logging();

    assert!(min_threads >= 1 && min_threads <= max_threads);
    assert!(max_threads * cpu_stride <= num_cpus::get());

    unsafe { nvalloc::thread::CPU_STRIDE = cpu_stride };

    let mut outfile = File::create(outfile).unwrap();
    writeln!(
        outfile,
        "alloc,threads,iteration,get_min,get_avg,get_max,put_min,put_avg,put_max,total"
    )
    .unwrap();

    let size = match size {
        0 => Size::L0,
        1 => Size::L1,
        2 => Size::L2,
        _ => panic!("`size` has to be 0, 1 or 2"),
    };

    let mem_pages = 2 * max_threads * MIN_PAGES;
    let mut mapping = mapping(0x1000_0000_0000, mem_pages, dax).unwrap();

    for threads in min_threads..=max_threads {
        for i in 0..iterations {
            let mapping = &mut mapping[..2 * threads * MIN_PAGES];

            let perf = bench_alloc::<TableAlloc>(mapping, Size::L0, threads);
            writeln!(outfile, "TableAlloc,{threads},{i},{perf}").unwrap();

            let perf = bench_alloc::<StackAlloc>(mapping, Size::L0, threads);
            writeln!(outfile, "StackAlloc,{threads},{i},{perf}").unwrap();

            if size == Size::L0 {
                let perf = bench_alloc::<LocalListAlloc>(mapping, Size::L0, threads);
                writeln!(outfile, "LocalListAlloc,{threads},{i},{perf}").unwrap();
            }

            if threads <= 1 && size == Size::L0 {
                let perf = bench_alloc::<MallocAlloc>(mapping, Size::L0, threads);
                writeln!(outfile, "MallocAlloc,{threads},{i},{perf}").unwrap();
            }

            if threads <= 1 && size == Size::L0 {
                let perf = bench_alloc::<BuddyAlloc>(mapping, Size::L0, threads);
                writeln!(outfile, "BuddyAlloc,{threads},{i},{perf}").unwrap();
            }
        }
    }
}

fn mapping<'a>(begin: usize, length: usize, dax: Option<String>) -> Result<MMap<'a, Page>, ()> {
    #[cfg(target_os = "linux")]
    if length > 0 {
        if let Some(file) = dax {
            warn!(
                "MMap file {file} l={}G",
                (length * std::mem::size_of::<Page>()) >> 30
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

fn bench_alloc<A: Alloc>(mapping: &mut [Page], size: Size, threads: usize) -> Perf {
    warn!("\n\n>>> bench {size:?} {}\n", type_name::<A>());
    // Allocate half the memory
    let allocs = mapping.len() / threads / 2 / Table::span(size as _);

    let timer = Instant::now();
    A::init(threads, mapping).unwrap();
    warn!("init time {}ms", timer.elapsed().as_millis());

    let barrier = Arc::new(Barrier::new(threads));

    let timer = Instant::now();
    let barrier = barrier.clone();
    let perfs = thread::parallel(threads as _, move |t| {
        thread::pin(t);
        barrier.wait();
        let mut pages = Vec::new();

        let mut get_min = u64::MAX;
        let mut get_max = 0;
        let mut get_avg = 0;

        for _ in 0..allocs {
            let timer = Cycles::now();
            match A::instance().get(t, size) {
                Ok(page) => pages.push(page),
                Err(Error::Memory) => break,
                Err(e) => panic!("{:?}", e),
            }
            let elapsed = timer.elapsed();
            get_min = get_min.min(elapsed);
            get_max = get_max.max(elapsed);
            get_avg += elapsed;
        }
        let len = pages.len() as u64;
        get_avg /= len;

        warn!("thread {t} allocated {len} [{get_min}, {get_avg}, {get_max}]",);
        barrier.wait();

        let mut put_min = u64::MAX;
        let mut put_max = 0;
        let mut put_avg = 0;

        for page in pages {
            let timer = Cycles::now();
            A::instance().put(t, page).unwrap();
            let elapsed = timer.elapsed();
            put_min = put_min.min(elapsed);
            put_max = put_max.max(elapsed);
            put_avg += elapsed;
        }
        put_avg /= len;
        warn!("thread {t} freed {len} [{put_min}, {put_avg}, {put_max}]",);

        let total = timer.elapsed().as_millis();
        warn!("time {total}ms");
        Perf {
            get_min,
            get_avg,
            get_max,
            put_min,
            put_avg,
            put_max,
            total,
        }
    });

    assert_eq!(A::instance().allocated_pages(), 0);

    A::destroy();

    let avg = Perf::avg(perfs.into_iter()).unwrap();
    warn!("{avg:#?}");
    avg
}

#[derive(Debug, Default)]
struct Perf {
    get_min: u64,
    get_avg: u64,
    get_max: u64,
    put_min: u64,
    put_avg: u64,
    put_max: u64,
    total: u128,
}

impl Perf {
    fn avg(iter: impl Iterator<Item = Perf>) -> Option<Perf> {
        let mut res = Perf::default();
        let mut counter = 0;
        for p in iter {
            res.get_min += p.get_min;
            res.get_avg += p.get_avg;
            res.get_max += p.get_max;
            res.put_min += p.put_min;
            res.put_avg += p.put_avg;
            res.put_max += p.put_max;
            res.total += p.total;
            counter += 1;
        }
        if counter > 0 {
            res.get_min /= counter;
            res.get_avg /= counter;
            res.get_max /= counter;
            res.put_min /= counter;
            res.put_avg /= counter;
            res.put_max /= counter;
            res.total /= counter as u128;
            Some(res)
        } else {
            None
        }
    }
}

impl fmt::Display for Perf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{},{},{},{},{},{},{}",
            self.get_min,
            self.get_avg,
            self.get_max,
            self.put_min,
            self.put_avg,
            self.put_max,
            self.total
        )
    }
}
