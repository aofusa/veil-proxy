//! veil-load: load generator for the veil benchmarks (`tools/perf/`, F-295).
//!
//! It is written for the questions `wrk` and `h2load` do not answer on a machine without Docker: a fixed offered
//! rate with coordinated-omission-corrected latency (`--rate`, like HttpArena's zrk), percentiles for HTTP/2 and
//! unary gRPC (`--mode grpc`), and the same output (`key=value` on one line, or `--tsv`) for all of them.
//!
//! One thread per `-t`, each with its own epoll loop and its share of the connections. See `tools/perf/README.md`.

mod hist;
mod h1;
mod h2;

use hist::Hist;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    H1,
    H2,
    Grpc,
}

#[derive(Clone)]
pub struct Args {
    pub target: String,
    pub host_header: String,
    pub paths: Vec<String>,
    pub headers: Vec<(String, String)>,
    pub method: String,
    pub body: Vec<u8>,
    pub conns: usize,
    pub threads: usize,
    pub duration: f64,
    pub warmup: f64,
    pub rate: f64,
    pub mode: Mode,
    pub streams: usize,
    pub pipeline: usize,
    pub pin: Vec<usize>,
    pub tsv: bool,
    pub label: String,
}

fn usage() -> ! {
    eprintln!(
        "usage: veil-load --target HOST:PORT --path /x [--path /y ...] [-c CONNS] [-t THREADS] [-d SECS]\n\
         \x20      [--warmup SECS] [--rate REQ_PER_SEC] [--mode h1|h2|grpc] [--streams N] [--pipeline N]\n\
         \x20      [--header 'K: V' ...] [--method M] [--body-file F] [--pin 2,3] [--tsv] [--label NAME]"
    );
    std::process::exit(2)
}

fn parse_cpus(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.split(',') {
        if let Some((a, b)) = part.split_once('-') {
            let (a, b): (usize, usize) = (a.parse().unwrap_or_else(|_| usage()), b.parse().unwrap_or_else(|_| usage()));
            out.extend(a..=b);
        } else {
            out.push(part.parse().unwrap_or_else(|_| usage()));
        }
    }
    out
}

fn parse_args() -> Args {
    let mut a = Args {
        target: String::new(),
        host_header: String::new(),
        paths: Vec::new(),
        headers: Vec::new(),
        method: "GET".into(),
        body: Vec::new(),
        conns: 64,
        threads: 2,
        duration: 10.0,
        warmup: 1.0,
        rate: 0.0,
        mode: Mode::H1,
        streams: 1,
        pipeline: 1,
        pin: Vec::new(),
        tsv: false,
        label: String::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match k.as_str() {
            "--target" => a.target = val(),
            "--path" => a.paths.push(val()),
            "--header" | "-H" => {
                let v = val();
                let (n, x) = v.split_once(':').unwrap_or_else(|| usage());
                a.headers.push((n.trim().to_string(), x.trim().to_string()));
            }
            "--method" => a.method = val(),
            "--body-file" => a.body = std::fs::read(val()).expect("body file"),
            "-c" | "--connections" => a.conns = val().parse().unwrap_or_else(|_| usage()),
            "-t" | "--threads" => a.threads = val().parse().unwrap_or_else(|_| usage()),
            "-d" | "--duration" => a.duration = val().parse().unwrap_or_else(|_| usage()),
            "--warmup" => a.warmup = val().parse().unwrap_or_else(|_| usage()),
            "--rate" => a.rate = val().parse().unwrap_or_else(|_| usage()),
            "--mode" => {
                a.mode = match val().as_str() {
                    "h1" => Mode::H1,
                    "h2" => Mode::H2,
                    "grpc" => Mode::Grpc,
                    _ => usage(),
                }
            }
            "--streams" | "-m" => a.streams = val().parse().unwrap_or_else(|_| usage()),
            "--pipeline" => a.pipeline = val().parse().unwrap_or_else(|_| usage()),
            "--pin" => a.pin = parse_cpus(&val()),
            "--tsv" => a.tsv = true,
            "--label" => a.label = val(),
            _ => usage(),
        }
    }
    if a.target.is_empty() || a.paths.is_empty() {
        usage();
    }
    a.host_header = a.target.clone();
    a.threads = a.threads.max(1);
    a.conns = a.conns.max(a.threads);
    a
}

/// What every worker thread reports.
pub struct Report {
    pub hist: Hist,
    pub ok: u64,
    pub errors: u64,
    pub non2xx: u64,
    pub bytes: u64,
    pub max_lag_ns: u64,
}

impl Report {
    pub fn new() -> Report {
        Report {
            hist: Hist::new(),
            ok: 0,
            errors: 0,
            non2xx: 0,
            bytes: 0,
            max_lag_ns: 0,
        }
    }
}

/// Shared run state: the workers record only between `start` and `end`.
pub struct Shared {
    pub stop: AtomicBool,
    pub recording: AtomicBool,
    pub connected: AtomicU64,
}

pub fn pin_current(cpu: usize) {
    // SAFETY: cpu_set_t は全ビット 0 で有効
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

pub fn connect_nb(addr: &std::net::SocketAddr) -> std::io::Result<RawFd> {
    use std::os::fd::IntoRawFd;
    let s = std::net::TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    s.set_nonblocking(true)?;
    Ok(s.into_raw_fd())
}

/// Offered schedule of one thread: request `n` is due at `start + n * interval`.
pub struct Schedule {
    pub interval_ns: f64,
    pub start: Instant,
    pub next: u64,
}

impl Schedule {
    pub fn new(rate_per_thread: f64, start: Instant) -> Schedule {
        Schedule {
            interval_ns: 1e9 / rate_per_thread,
            start,
            next: 0,
        }
    }
    #[inline]
    pub fn due_ns(&self) -> u64 {
        (self.next as f64 * self.interval_ns) as u64
    }
}

#[inline]
pub fn now_ns(base: Instant) -> u64 {
    base.elapsed().as_nanos() as u64
}

/// How long a closed-loop thread (or a fixed-rate thread with nothing left to send) waits for events.
pub const IDLE_WAIT_NS: u64 = 100_000_000;

/// Waits for events for at most `timeout_ns` (`epoll_wait` has only millisecond resolution, and two requests of
/// one thread are microseconds apart).
///
/// 固定レートも次の予定まで眠る。回し続けると、ループバックで相手のスレッドを起こす費用がサーバの `send` から
/// 消えて（システム時間が半分になる）、眠る普通のクライアントを相手にした計測より大幅に軽く見える
pub fn wait_events(ep: RawFd, events: &mut [libc::epoll_event], timeout_ns: u64) -> i32 {
    let ts = libc::timespec {
        tv_sec: (timeout_ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (timeout_ns % 1_000_000_000) as _,
    };
    // SAFETY: events と ts は呼び出しの間有効な領域で、シグナルマスクは渡さない
    unsafe {
        libc::syscall(
            libc::SYS_epoll_pwait2,
            ep,
            events.as_mut_ptr(),
            events.len() as libc::c_int,
            &ts as *const libc::timespec,
            std::ptr::null::<libc::sigset_t>(),
            0usize,
        ) as i32
    }
}

fn main() {
    let args = parse_args();
    let addr: std::net::SocketAddr = {
        use std::net::ToSocketAddrs;
        args.target
            .to_socket_addrs()
            .expect("resolve target")
            .next()
            .expect("no address")
    };
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        recording: AtomicBool::new(false),
        connected: AtomicU64::new(0),
    });
    let base = Instant::now();
    let mut handles = Vec::new();
    for t in 0..args.threads {
        let a = args.clone();
        let s = shared.clone();
        let conns = a.conns / a.threads + usize::from(t < a.conns % a.threads);
        handles.push(std::thread::spawn(move || {
            if !a.pin.is_empty() {
                pin_current(a.pin[t % a.pin.len()]);
            }
            match a.mode {
                Mode::H1 => h1::run(&a, addr, conns, t, &s, base),
                Mode::H2 | Mode::Grpc => h2::run(&a, addr, conns, t, &s, base),
            }
        }));
    }
    std::thread::sleep(Duration::from_secs_f64(args.warmup));
    shared.recording.store(true, Ordering::SeqCst);
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(args.duration));
    shared.recording.store(false, Ordering::SeqCst);
    let elapsed = start.elapsed().as_secs_f64();
    shared.stop.store(true, Ordering::SeqCst);
    let mut total = Report::new();
    for h in handles {
        let r = h.join().expect("worker");
        total.hist.merge(&r.hist);
        total.ok += r.ok;
        total.errors += r.errors;
        total.non2xx += r.non2xx;
        total.bytes += r.bytes;
        total.max_lag_ns = total.max_lag_ns.max(r.max_lag_ns);
    }
    let rps = total.ok as f64 / elapsed;
    let us = |v: u64| v as f64 / 1000.0;
    let h = &total.hist;
    let ratio = if args.rate > 0.0 { rps / args.rate } else { 0.0 };
    if args.tsv {
        println!(
            "{}\t{:.0}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.4}\t{}\t{}\t{:.0}",
            args.label,
            rps,
            us(h.mean()),
            us(h.percentile(50.0)),
            us(h.percentile(90.0)),
            us(h.percentile(99.0)),
            us(h.percentile(99.9)),
            us(h.max_ns),
            ratio,
            total.errors,
            total.non2xx,
            total.bytes as f64 / total.ok.max(1) as f64,
        );
    } else {
        println!(
            "rps={:.0} mean_us={:.1} p50_us={:.1} p90_us={:.1} p99_us={:.1} p999_us={:.1} max_us={:.1} rate_ratio={:.4} errors={} non2xx={} bytes_per_resp={:.0} max_lag_us={:.1}",
            rps,
            us(h.mean()),
            us(h.percentile(50.0)),
            us(h.percentile(90.0)),
            us(h.percentile(99.0)),
            us(h.percentile(99.9)),
            us(h.max_ns),
            ratio,
            total.errors,
            total.non2xx,
            total.bytes as f64 / total.ok.max(1) as f64,
            us(total.max_lag_ns),
        );
    }
}
