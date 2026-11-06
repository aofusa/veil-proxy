//! HTTP/2 (prior knowledge) client loop, for plain requests (`--mode h2`) and unary gRPC (`--mode grpc`).
//!
//! The header block is built once with literals that do not touch the dynamic table, so every stream sends the
//! same bytes. Responses are not HPACK-decoded: a stream finishes on `END_STREAM` and `:status: 200` is recognized
//! from the indexed form the server sends; correctness is what the E2E tests are for.

use crate::{connect_nb, now_ns, Args, Mode, Report, Schedule, Shared};
use std::os::fd::RawFd;
use std::sync::atomic::Ordering;
use std::time::Instant;

const FRAME_DATA: u8 = 0;
const FRAME_HEADERS: u8 = 1;
const FRAME_RST: u8 = 3;
const FRAME_SETTINGS: u8 = 4;
const FRAME_PING: u8 = 6;
const FRAME_GOAWAY: u8 = 7;
const FLAG_END_STREAM: u8 = 1;
const FLAG_END_HEADERS: u8 = 4;
const FLAG_ACK: u8 = 1;

fn literal(out: &mut Vec<u8>, index: u8, value: &str) {
    // リテラル（インデックスしない）: 名前はスタティックテーブルの添字
    if index < 15 {
        out.push(index);
    } else {
        out.push(0x0f);
        out.push(index - 15);
    }
    string(out, value);
}

fn string(out: &mut Vec<u8>, s: &str) {
    if s.len() < 127 {
        out.push(s.len() as u8);
    } else {
        out.push(127);
        let mut n = s.len() - 127;
        while n >= 128 {
            out.push((n % 128) as u8 | 0x80);
            n /= 128;
        }
        out.push(n as u8);
    }
    out.extend_from_slice(s.as_bytes());
}

fn literal_name(out: &mut Vec<u8>, name: &str, value: &str) {
    out.push(0);
    string(out, name);
    string(out, value);
}

fn header_block(a: &Args, path: &str) -> Vec<u8> {
    let mut b = Vec::new();
    // :method（GET=2、POST=3）・:scheme http（6）
    b.push(if a.mode == Mode::Grpc || !a.body.is_empty() || a.method == "POST" {
        0x83
    } else {
        0x82
    });
    b.push(0x86);
    literal(&mut b, 4, path); // :path
    literal(&mut b, 1, &a.host_header); // :authority
    if a.mode == Mode::Grpc {
        literal_name(&mut b, "content-type", "application/grpc");
        literal_name(&mut b, "te", "trailers");
    }
    for (n, v) in &a.headers {
        literal_name(&mut b, &n.to_ascii_lowercase(), v);
    }
    b
}

fn frame_head(out: &mut Vec<u8>, len: usize, ty: u8, flags: u8, stream: u32) {
    out.extend_from_slice(&(len as u32).to_be_bytes()[1..]);
    out.push(ty);
    out.push(flags);
    out.extend_from_slice(&stream.to_be_bytes());
}

fn grpc_body(a: &Args) -> Vec<u8> {
    if a.mode != Mode::Grpc {
        return a.body.clone();
    }
    // 引数がそのままの要求（5 バイトの前置きを含む）ならそのまま使う
    a.body.clone()
}

struct Conn {
    fd: RawFd,
    buf: Vec<u8>,
    len: usize,
    next_stream: u32,
    /// Start time per stream slot (`(id / 2) % slots`).
    start: Vec<u64>,
    active: usize,
    /// Pending writes.
    out: Vec<u8>,
    recv_window_used: usize,
    next_path: usize,
    /// `:status` of each stream was not 200.
    bad: Vec<bool>,
    dead: bool,
}

fn flush(c: &mut Conn) -> bool {
    if c.out.is_empty() {
        return true;
    }
    // SAFETY: fd は開いていて out は有効な領域
    let n = unsafe { libc::write(c.fd, c.out.as_ptr().cast(), c.out.len()) };
    if n < 0 {
        return std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock;
    }
    c.out.drain(..n as usize);
    true
}

fn start_stream(c: &mut Conn, blocks: &[Vec<u8>], body: &[u8], now: u64, slots: usize) {
    let id = c.next_stream;
    c.next_stream += 2;
    let block = &blocks[c.next_path];
    c.next_path = (c.next_path + 1) % blocks.len();
    let end = if body.is_empty() { FLAG_END_STREAM } else { 0 };
    frame_head(&mut c.out, block.len(), FRAME_HEADERS, FLAG_END_HEADERS | end, id);
    c.out.extend_from_slice(block);
    if !body.is_empty() {
        frame_head(&mut c.out, body.len(), FRAME_DATA, FLAG_END_STREAM, id);
        c.out.extend_from_slice(body);
    }
    c.start[(id as usize / 2) % slots] = now;
    c.bad[(id as usize / 2) % slots] = false;
    c.active += 1;
}

pub fn run(
    a: &Args,
    addr: std::net::SocketAddr,
    nconns: usize,
    tid: usize,
    s: &Shared,
    base: Instant,
) -> Report {
    let mut rep = Report::new();
    let blocks: Vec<Vec<u8>> = a.paths.iter().map(|p| header_block(a, p)).collect();
    let body = grpc_body(a);
    let slots = (a.streams.max(1) * 2).next_power_of_two();
    // SAFETY: 引数は固定
    let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    let mut conns: Vec<Conn> = Vec::with_capacity(nconns);
    for i in 0..nconns {
        let fd = match connect_nb(&addr) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("connect: {}", e);
                rep.errors += 1;
                continue;
            }
        };
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: i as u64,
        };
        // SAFETY: fd と ev は有効
        unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut ev) };
        let mut out = Vec::with_capacity(16 * 1024);
        out.extend_from_slice(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        // SETTINGS: INITIAL_WINDOW_SIZE = 2^30（受信側の窓は気にしない）
        frame_head(&mut out, 6, FRAME_SETTINGS, 0, 0);
        out.extend_from_slice(&[0, 4, 0x40, 0, 0, 0]);
        // 接続の窓も広げる
        frame_head(&mut out, 4, 8, 0, 0);
        out.extend_from_slice(&0x3fff_0000u32.to_be_bytes());
        let c = Conn {
            fd,
            buf: vec![0; 256 * 1024],
            len: 0,
            next_stream: 1,
            start: vec![0; slots],
            active: 0,
            out,
            recv_window_used: 0,
            next_path: (tid + i) % blocks.len(),
            bad: vec![false; slots],
            dead: false,
        };
        conns.push(c);
    }
    let fixed = a.rate > 0.0;
    let per_thread = a.rate / a.threads as f64;
    let mut sched = Schedule::new(per_thread.max(1.0), Instant::now());
    let sched_base = sched.start;
    if !fixed {
        for c in conns.iter_mut() {
            for _ in 0..a.streams.max(1) {
                start_stream(c, &blocks, &body, now_ns(base), slots);
            }
            flush(c);
        }
    } else {
        for c in conns.iter_mut() {
            flush(c);
        }
    }
    let mut rr = 0usize;
    let mut events: Vec<libc::epoll_event> = vec![libc::epoll_event { events: 0, u64: 0 }; 1024];
    while !s.stop.load(Ordering::Relaxed) {
        if fixed && !conns.is_empty() {
            let now = now_ns(sched_base);
            while sched.due_ns() <= now {
                // 空きのある接続を順に探す
                let mut found = None;
                for _ in 0..conns.len() {
                    rr = (rr + 1) % conns.len();
                    if !conns[rr].dead && conns[rr].active < a.streams.max(1) {
                        found = Some(rr);
                        break;
                    }
                }
                let Some(i) = found else { break };
                let due = sched.due_ns();
                sched.next += 1;
                rep.max_lag_ns = rep.max_lag_ns.max(now - due);
                start_stream(&mut conns[i], &blocks, &body, due, slots);
                flush(&mut conns[i]);
            }
        }
        let wait = if fixed { 0 } else { 100 };
        // SAFETY: events は有効な領域
        let n = unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), events.len() as i32, wait) };
        if n < 0 {
            continue;
        }
        let record = s.recording.load(Ordering::Relaxed);
        for ei in 0..n as usize {
            let idx = events[ei].u64 as usize;
            let c = &mut conns[idx];
            if c.len == c.buf.len() {
                c.buf.resize(c.buf.len() * 2, 0);
            }
            // SAFETY: バッファの残りへ読む
            let r = unsafe { libc::read(c.fd, c.buf[c.len..].as_mut_ptr().cast(), c.buf.len() - c.len) };
            if r == 0 {
                rep.errors += 1;
                c.dead = true;
                let mut ev = libc::epoll_event { events: 0, u64: 0 };
                // SAFETY: 閉じた接続を外す
                unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_DEL, c.fd, &mut ev) };
                continue;
            }
            if r < 0 {
                continue;
            }
            c.len += r as usize;
            let mut off = 0;
            let mut finished = 0usize;
            while c.len - off >= 9 {
                let b = &c.buf[off..];
                let flen = ((b[0] as usize) << 16) | ((b[1] as usize) << 8) | b[2] as usize;
                if c.len - off < 9 + flen {
                    break;
                }
                let ty = b[3];
                let flags = b[4];
                let sid = u32::from_be_bytes([b[5] & 0x7f, b[6], b[7], b[8]]);
                let mut done = false;
                match ty {
                    FRAME_SETTINGS => {
                        if flags & FLAG_ACK == 0 {
                            let mut ack = Vec::with_capacity(9);
                            frame_head(&mut ack, 0, FRAME_SETTINGS, FLAG_ACK, 0);
                            c.out.extend_from_slice(&ack);
                        }
                    }
                    FRAME_PING => {
                        if flags & FLAG_ACK == 0 {
                            let mut f = Vec::with_capacity(17);
                            frame_head(&mut f, 8, FRAME_PING, FLAG_ACK, 0);
                            f.extend_from_slice(&b[9..17]);
                            c.out.extend_from_slice(&f);
                        }
                    }
                    FRAME_GOAWAY => {
                        rep.errors += 1;
                        c.dead = true;
                    }
                    FRAME_RST => {
                        rep.errors += 1;
                        done = true;
                    }
                    FRAME_HEADERS => {
                        // 最初のヘッダブロック: `:status: 200` はスタティックテーブルの 8 番（0x88）
                        let pad = usize::from(flags & 8 != 0);
                        let prio = if flags & 0x20 != 0 { 5 } else { 0 };
                        let first = b.get(9 + pad + prio).copied().unwrap_or(0);
                        let slot = (sid as usize / 2) % slots;
                        if first & 0x80 != 0 && first != 0x88 && first < 0xbe && flags & FLAG_END_STREAM == 0 {
                            c.bad[slot] = true;
                        }
                        if record {
                            rep.bytes += flen as u64;
                        }
                        if flags & FLAG_END_STREAM != 0 {
                            done = true;
                        }
                    }
                    FRAME_DATA => {
                        c.recv_window_used += flen;
                        if record {
                            rep.bytes += flen as u64;
                        }
                        if flags & FLAG_END_STREAM != 0 {
                            done = true;
                        }
                    }
                    _ => {}
                }
                if done {
                    let slot = (sid as usize / 2) % slots;
                    if record {
                        let now = if fixed { now_ns(sched_base) } else { now_ns(base) };
                        rep.hist.record(now.saturating_sub(c.start[slot]));
                        rep.ok += 1;
                        if c.bad[slot] {
                            rep.non2xx += 1;
                        }
                    }
                    c.active -= 1;
                    finished += 1;
                }
                off += 9 + flen;
            }
            if off > 0 {
                c.buf.copy_within(off..c.len, 0);
                c.len -= off;
            }
            if c.recv_window_used > 1 << 28 {
                let n = c.recv_window_used as u32;
                c.recv_window_used = 0;
                let mut f = Vec::with_capacity(13);
                frame_head(&mut f, 4, 8, 0, 0);
                f.extend_from_slice(&n.to_be_bytes());
                c.out.extend_from_slice(&f);
            }
            if !fixed && !c.dead {
                for _ in 0..finished {
                    start_stream(c, &blocks, &body, now_ns(base), slots);
                }
            }
            if !flush(c) {
                rep.errors += 1;
                c.dead = true;
            }
        }
    }
    for c in &conns {
        // SAFETY: 終了時に閉じる
        unsafe { libc::close(c.fd) };
    }
    // SAFETY: epoll の fd を閉じる
    unsafe { libc::close(ep) };
    rep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_encodes_long_index() {
        let mut v = Vec::new();
        literal(&mut v, 4, "/x");
        assert_eq!(v, vec![4, 2, b'/', b'x']);
        let mut v = Vec::new();
        literal(&mut v, 31, "a");
        assert_eq!(v, vec![0x0f, 16, 1, b'a']);
    }
}
