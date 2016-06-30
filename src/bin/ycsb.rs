#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_mut)]
#![allow(dead_code)]

/// Code ported from RAMCloud src/ClusterPerf.cc::ZipfianGenerator

extern crate rand; // import before nibble
#[macro_use]
extern crate log;
extern crate time;
extern crate clap;
extern crate num;

extern crate nibble;

use clap::{Arg, App, SubCommand};
use log::LogLevel;
use nibble::clock;
use nibble::common::{ErrorCode,rdrand,rdrandq};
use nibble::cuckoo;
use nibble::epoch;
use nibble::logger;
use nibble::memory;
use nibble::nib::{PutPolicy,Nibble};
use nibble::numa::{self,NodeId};
use nibble::sched::*;
use nibble::segment::{ObjDesc,SEGMENT_SIZE};
use rand::Rng;
use std::collections::VecDeque;
use std::mem;
use std::sync::Arc;
use std::sync::atomic::*;
use std::thread::{self,JoinHandle};
use std::time::{Instant,Duration};

trait DistGenerator {
    fn next(&self) -> u64;
}

#[derive(Debug,Clone,Copy)]
struct Zipfian {
    n: u64,
    theta: f64,
    alpha: f64,
    zetan: f64,
    eta: f64,
}

impl Zipfian {

    pub fn new(n: u64, s: f64) -> Self {
        let theta: f64 = s;
        let zetan: f64 = Self::zeta(n, theta);
        Zipfian {
            n: n, theta: theta,
            alpha: 1f64 / (1f64 - theta),
            zetan: zetan,
            eta: (1f64 - (2f64 / (n as f64)).powf(1f64-theta)) /
                (1f64 - Self::zeta(2u64, theta) / zetan),
        }
    }

    /// Compute H(N,s), the generalized Nth harmonic number
    pub fn zeta(n: u64, theta: f64) -> f64 {
        let mut sum: f64 = 0f64;
        for x in 0u64..n {
            sum += 1f64 / ((x+1) as f64).powf(theta);
        }
        sum
    }
}

impl DistGenerator for Zipfian {

    #[inline(always)]
    fn next(&self) -> u64 {
        let u: f64 = unsafe { rdrandq() as f64 } / 
            (std::u64::MAX as f64);
        let uz: f64 = u * self.zetan;
        if uz < 1f64 { 0u64 }
        else if uz < (1f64 + 0.5f64.powf(self.theta)) { 1u64 }
        else {
            ((self.eta*u - self.eta + 1f64).powf(self.alpha)
                * (self.n as f64)) as u64
        }
    }
}

struct Uniform {
    n: u64,
}

impl Uniform {

    pub fn new(n: u64) -> Self {
        Uniform { n: n }
    }
}

impl DistGenerator for Uniform {

    #[inline(always)]
    fn next(&self) -> u64 {
        // There might be a faster way if n is smaller, instead of
        // invoking rdrandq each time and performing modulus, e.g., if
        // n is 128-255 inclusive, we can take a 64bit rdrandq and
        // use it 8 times (use 8 bits for each before calling rdrandq
        // again). It's likely we'll have < 2^32 keys (4bil.) in
        // typical cases, so could reuse at least 2x.
        unsafe { rdrandq() % self.n }
    }
}

#[derive(Debug,Clone,Copy)]
enum YCSB {
    A, B, C, WR,
}

struct WorkloadGenerator {
    nibble: Nibble,
    config: Config,
    gen: Box<DistGenerator>,
}

impl WorkloadGenerator {

    pub fn new(config: Config) -> Self {
        let n = config.records;
        let mut nibble = Nibble::new( 1usize << 30 );
        nibble.enable_compaction(NodeId(0));
        info!("WorkloadGenerator {:?}", config);
        WorkloadGenerator {
            nibble: nibble,
            config: config,
            gen: match config.dist {
                Dist::Zipfian(s) => Box::new(
                    Zipfian::new(n as u64,s)),
                Dist::Uniform => Box::new(
                    Uniform::new(n as u64)),
            },
        }
    }

    pub fn setup(&mut self) {
        let size = self.config.size;
        let value = memory::allocate::<u8>(size);
        let v = Some(value as *const u8);

        // FIXME change this if we scale up?
        let policy = PutPolicy::Specific(0);

        info!("Inserting {} objects of size {}",
              self.config.records, self.config.size);
        for record in 0..(self.config.records as u64) {
            let obj = ObjDesc::new(record, v, size as u32);
            if let Err(e) = self.nibble.put_where(&obj, policy) {
                panic!("Error {:?}", e);
            }
        }
        unsafe { memory::deallocate(value, size); }
    }

    /// Run at specified op per second (ops).
    /// Zero means no throttling.
    pub fn run(&mut self) {
        let read_threshold =
            (std::u64::MAX / 100u64) * (self.config.read_pct as u64);

        let size = self.config.size;
        let value = memory::allocate::<u8>(size);
        let v = Some(value as *const u8);

        // each op should have this latency (in nsec) or less
        let nspo: u64 = match self.config.ops {
            0u64 => 0u64,
            o => 1_000_000_000u64 / o,
        };
        // and the equivalent in cycles
        let cpo = clock::from_nano(nspo);
        debug!("nspo {} cpo {}", nspo, cpo);

        // FIXME change this if we scale up?
        let policy = PutPolicy::Specific(0);

        info!("Starting experiment");
        let mut counter = 0u64;
        let start = unsafe { clock::rdtsc() }; // for throttling

        let mut tic = unsafe { clock::rdtsc() }; // report performance
        let mut per_loop = 0u64; // ops performed per report

        loop {
            // since we don't delete, all reads will hit
            let key = self.gen.next() %
                (self.config.records as u64);
            let rd = unsafe { rdrandq() };

            if rd < read_threshold {
                if let (Err(e),_) = self.nibble.get_object(key) {
                    panic!("Error: {:?}", e);
                }
            } else {
                let obj = ObjDesc::new(key, v, size as u32);
                if let Err(e) = self.nibble.put_where(&obj, policy) {
                    panic!("Error {:?}", e);
                }
            }

            counter += 1;
            per_loop += 1;

            // throttle
            if cpo > 0 {
                let next = start + counter * cpo;
                while unsafe { clock::rdtsc() } < next { ; }
            }

            let toc = unsafe { clock::rdtsc() };
            if clock::to_seconds(toc - tic) > 1 {
                let tim = clock::to_secondsf(toc-tic);
                let kops = (per_loop as f64 / 1000f64) / tim;
                println!("{:.3} kops/sec", kops);
                // reset
                tic = unsafe { clock::rdtsc() };
                per_loop = 0u64;
            }
        }

        //unsafe { memory::deallocate(value, size); }
    }
}

#[derive(Debug,Clone,Copy)]
enum Dist {
    /// Contained value is s (exponent modifier)
    Zipfian(f64),
    Uniform,
}

// TODO: setup configuration, how to allocate objects across sockets
#[derive(Debug,Clone,Copy)]
struct Config {
    /// Amount of memory to use for nibble
    mem: usize,
    /// if None, custom workload
    ycsb: Option<YCSB>,
    /// number of objects
    records: usize,
    /// size of each object
    size: usize,
    dist: Dist,
    /// 0-100 (1-read_pct = write pct)
    read_pct: usize,
    /// operations per second to sustain. 0 = unthrottled
    ops: u64,
}

impl Config {

    pub fn ycsb(mem: usize, ops: u64, w: YCSB) -> Self {
        let rc: usize = 1000; // default
        Self::ycsb_more(mem, ops, w, rc)
    }

    // more records
    pub fn ycsb_more(mem: usize, ops: u64, w: YCSB, records: usize) -> Self {
        let rs: usize = 100;
        let rp: usize = match w {
            YCSB::A => 50,
            YCSB::B => 95,
            YCSB::C => 100,
            YCSB::WR => 0,
        };
        Config {
            mem: mem,
            ycsb: Some(w),
            records: records,
            size: rs,
            dist: Dist::Zipfian(0.99f64),
            read_pct: rp,
            ops: ops,
        }
    }

    // directly construct it
    pub fn custom(mem: usize, ops: u64, records: usize,
                  size: usize, dist: Dist,
                  read_pct: usize) -> Self {
        Config {
            mem: mem,
            ycsb: None,
            records: records,
            size: size,
            dist: dist,
            read_pct: read_pct,
            ops: ops,
        }
    }
}

/// Convert an argument as number.
fn arg_as_num<T: num::Integer>(args: &clap::ArgMatches,
                               name: &str) -> T {
    match args.value_of(name) {
        None => panic!("Specify {}", name),
        Some(s) => match T::from_str_radix(s,10) {
            Err(_) => panic!("size NaN"),
            Ok(s) => s,
        },
    }
}

fn main() {
    logger::enable();

    let matches = App::new("ycsb")
        .arg(Arg::with_name("ycsb")
             .long("ycsb")
             .takes_value(true))
        .arg(Arg::with_name("size")
             .long("size")
             .takes_value(true))
        .arg(Arg::with_name("capacity")
             .long("capacity")
             .takes_value(true))
        .arg(Arg::with_name("records")
             .long("records")
             .takes_value(true))
        .arg(Arg::with_name("readpct")
             .long("readpct")
             .takes_value(true))
        .arg(Arg::with_name("dist")
             .long("dist")
             .takes_value(true))
        .arg(Arg::with_name("ops")
             .long("ops")
             .takes_value(true))
        .get_matches();

    let config = match matches.value_of("ycsb") {

        // Custom Configuration
        None => {
            let size     = arg_as_num::<usize>(&matches, "size");
            let capacity = arg_as_num::<usize>(&matches, "capacity");
            let ops      = arg_as_num::<u64>(&matches, "ops");
            let records  = arg_as_num::<usize>(&matches, "records");
            let readpct  = arg_as_num::<usize>(&matches, "readpct");

            let dist = match matches.value_of("dist") {
                None => panic!("Specify dist"),
                Some(s) => {
                    if s == "zipfian" {
                        // s value from YCSB itself
                        Dist::Zipfian(0.99_f64)
                    } else if s == "uniform" {
                        Dist::Uniform
                    } else {
                        panic!("unknown distribution");
                    }
                },
            };

            Config::custom(capacity, ops, records,
                           size, dist, readpct)
        },

        // YCSB-Specific Configuration
        Some(s) => {
            let ycsb = match s {
                "A" => YCSB::A,
                "B" => YCSB::B,
                "C" => YCSB::C,
                "WR" => YCSB::WR,
                _ => panic!("unknown YCSB configuration"),
            };

            let capacity = arg_as_num::<usize>(&matches, "capacity");
            let ops      = arg_as_num::<u64>(&matches, "ops");

            // optional argument
            let records = match matches.value_of("records") {
                None => None,
                Some(s) => match usize::from_str_radix(s,10) {
                    Err(_) => panic!("records NaN"),
                    Ok(s) => Some(s),
                },
            };

            match records {
                None => Config::ycsb(capacity, ops, ycsb),
                Some(r) => Config::ycsb_more(capacity,
                                          ops, ycsb, r),
            }
        },
    };

    // default size 1KiB

    // A rc=1000 size=1kb 50:50 zipfian
    // B rc=1000 size=1kb 95:05 zipfian
    // C rc=1000 size=1kb  1:0  zipfian

    // W rc=1000 size=1kb  0:1  zipfian

    // or user-defined if you omit --ycsb
    // --records --size --readpct --dist --capacity --ops

    let mut gen = WorkloadGenerator::new(config);
    gen.setup();
    gen.run();
}
