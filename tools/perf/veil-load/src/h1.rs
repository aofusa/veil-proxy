//! HTTP/1.1 client loop (closed-loop, pipelined or fixed rate).

use crate::{connect_nb, now_ns, Args, Report, Schedule, Shared};
use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::sync::atomic::Ordering;
use std::time::Instant;

struct Conn {
    fd: RawFd,
    buf: Vec<u8>,
    len: usize,
    /// Send time of the requests in flight, oldest first.
    inflight: VecDeque<u64>,
    /// Bytes of the current response body still to skip.
    skip: usize,
    /// In a chunked body.
    chunked: bool,
    /// Rotation over the request templates.
    next: usize,
    status_ok: bool,
    /// Bytes of a partly written request.
    pending: Vec<u8>,
}

fn build_requests(a: &Args) -> Vec<Vec<u8>> {
    a.paths
        .iter()
        .map(|p| {
            let mut r = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", a.method, p, a.host_header);
            for (n, v) in &a.headers {
                r.push_str(&format!("{}: {}\r\n", n, v));
            }
            if !a.body.is_empty() {
                r.push_str(&format!("Content-Length: {}\r\n", a.body.len()));
            }
            r.push_str("\r\n");
            let mut b = r.into_bytes();
            b.extend_from_slice(&a.body);
            b
        })
        .collect()
}

fn find_crlf2(b: &[u8]) -> Option<usize> {
    let mut i = 3;
    while i < b.len() {
        match b[i] {
            b'\n' => {
                if b[i - 1] == b'\r' && b[i - 2] == b'\n' && b[i - 3] == b'\r' {
                    return Some(i + 1);
                }
                i += 1;
            }
            b'\r' => i += 1,
            _ => i += 4.min(b.len() - i).max(1),
        }
    }
    None
}

// 先頭から `\r\n\r\n` までのヘッダを見て本文の長さを返す
fn head_info(h: &[u8]) -> (usize, bool, bool) {
    let ok = h.len() > 12 && h[9] == b'2';
    let mut cl = 0usize;
    let mut chunked = false;
    let mut i = 0;
    while i + 1 < h.len() {
        // 行の先頭
        let c = h[i] | 0x20;
        if c == b'c' && h.len() - i > 15 && h[i..i + 15].eq_ignore_ascii_case(b"content-length:") {
            let mut j = i + 15;
            while h[j] == b' ' {
                j += 1;
            }
            cl = 0;
            while h[j].is_ascii_digit() {
                cl = cl * 10 + (h[j] - b'0') as usize;
                j += 1;
            }
        } else if c == b't'
            && h.len() - i > 26
            && h[i..i + 18].eq_ignore_ascii_case(b"transfer-encoding:")
            && h[i..].iter().take(40).position(|&x| x == b'\r').is_some_and(|e| {
                h[i..i + e].windows(7).any(|w| w.eq_ignore_ascii_case(b"chunked"))
            })
        {
            chunked = true;
        }
        match h[i..].iter().position(|&x| x == b'\n') {
            Some(p) => i += p + 1,
            None => break,
        }
    }
    (cl, chunked, ok)
}

// chunked の本文を飛ばす: 全部読めたら消費したバイト数を返す
fn skip_chunked(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    loop {
        let line_end = b[i..].windows(2).position(|w| w == b"\r\n")?;
        let mut n = 0usize;
        for &c in &b[i..i + line_end] {
            match (c as char).to_digit(16) {
                Some(d) => n = n * 16 + d as usize,
                None => break,
            }
        }
        i += line_end + 2;
        if n == 0 {
            // トレーラは無い前提（最後の CRLF）
            if b.len() < i + 2 {
                return None;
            }
            return Some(i + 2);
        }
        if b.len() < i + n + 2 {
            return None;
        }
        i += n + 2;
    }
}

fn write_all(c: &mut Conn, r: &[u8]) -> bool {
    if !c.pending.is_empty() {
        c.pending.extend_from_slice(r);
        return true;
    }
    // SAFETY: fd は開いていて r は有効な領域
    let n = unsafe { libc::write(c.fd, r.as_ptr().cast(), r.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            c.pending.extend_from_slice(r);
            return true;
        }
        return false;
    }
    if (n as usize) < r.len() {
        c.pending.extend_from_slice(&r[n as usize..]);
    }
    true
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
    let reqs = build_requests(a);
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
            events: (libc::EPOLLIN) as u32,
            u64: i as u64,
        };
        // SAFETY: fd と ev は有効
        unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut ev) };
        conns.push(Conn {
            fd,
            buf: vec![0; 64 * 1024],
            len: 0,
            inflight: VecDeque::with_capacity(a.pipeline.max(1)),
            skip: 0,
            chunked: false,
            next: (tid + i) % reqs.len(),
            status_ok: true,
            pending: Vec::new(),
        });
    }
    let fixed = a.rate > 0.0;
    let per_thread = a.rate / a.threads as f64;
    let mut sched = Schedule::new(per_thread.max(1.0), Instant::now());
    let sched_base = sched.start;
    let mut free: Vec<usize> = Vec::with_capacity(conns.len());
    if fixed {
        free.extend(0..conns.len());
    } else {
        let depth = a.pipeline.max(1);
        for i in 0..conns.len() {
            for _ in 0..depth {
                let c = &mut conns[i];
                let r = &reqs[c.next];
                c.next = (c.next + 1) % reqs.len();
                c.inflight.push_back(now_ns(base));
                if !write_all(c, r) {
                    rep.errors += 1;
                }
            }
        }
    }
    let mut events: Vec<libc::epoll_event> = vec![libc::epoll_event { events: 0, u64: 0 }; 1024];
    while !s.stop.load(Ordering::Relaxed) {
        // 固定レート: 予定の時刻を過ぎた分を空いている接続へ出す
        if fixed {
            let now = now_ns(sched_base);
            while sched.due_ns() <= now {
                let Some(i) = free.pop() else { break };
                let due = sched.due_ns();
                sched.next += 1;
                let lag = now - due;
                if lag > rep.max_lag_ns {
                    rep.max_lag_ns = lag;
                }
                let c = &mut conns[i];
                let r = &reqs[c.next];
                c.next = (c.next + 1) % reqs.len();
                c.inflight.push_back(due);
                if !write_all(c, r) {
                    rep.errors += 1;
                }
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
            // 読む
            loop {
                if c.len == c.buf.len() {
                    c.buf.resize(c.buf.len() * 2, 0);
                }
                // SAFETY: バッファの残りへ読む
                let r = unsafe { libc::read(c.fd, c.buf[c.len..].as_mut_ptr().cast(), c.buf.len() - c.len) };
                if r == 0 {
                    rep.errors += 1;
                    let mut ev = libc::epoll_event { events: 0, u64: 0 };
                    // SAFETY: 閉じた接続を外す
                    unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_DEL, c.fd, &mut ev) };
                    break;
                }
                if r < 0 {
                    break;
                }
                c.len += r as usize;
                if (r as usize) < c.buf.len() - (c.len - r as usize) {
                    break;
                }
            }
            // 応答を切り出す
            let mut off = 0;
            loop {
                if c.skip > 0 {
                    let take = c.skip.min(c.len - off);
                    off += take;
                    c.skip -= take;
                    if c.skip > 0 {
                        break;
                    }
                } else if c.chunked {
                    match skip_chunked(&c.buf[off..c.len]) {
                        Some(n) => {
                            off += n;
                            c.chunked = false;
                        }
                        None => break,
                    }
                } else {
                    let Some(h) = find_crlf2(&c.buf[off..c.len]) else { break };
                    let (cl, chunked, ok) = head_info(&c.buf[off..off + h]);
                    off += h;
                    c.status_ok = ok;
                    c.chunked = chunked;
                    c.skip = cl;
                    if record {
                        rep.bytes += (h + cl) as u64;
                    }
                    if cl > 0 || chunked {
                        continue;
                    }
                }
                // 1 応答の完了
                let start = c.inflight.pop_front().unwrap_or(0);
                if record {
                    let now = if fixed { now_ns(sched_base) } else { now_ns(base) };
                    rep.hist.record(now.saturating_sub(start));
                    rep.ok += 1;
                    if !c.status_ok {
                        rep.non2xx += 1;
                    }
                }
                if fixed {
                    if c.inflight.is_empty() {
                        free.push(idx);
                    }
                } else {
                    let r = &reqs[c.next];
                    c.next = (c.next + 1) % reqs.len();
                    c.inflight.push_back(now_ns(base));
                    if !write_all(c, r) {
                        rep.errors += 1;
                    }
                }
                if off == c.len {
                    break;
                }
            }
            if off > 0 {
                c.buf.copy_within(off..c.len, 0);
                c.len -= off;
            }
            if !c.pending.is_empty() {
                let p = std::mem::take(&mut c.pending);
                write_all(c, &p);
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
    fn finds_the_head_end() {
        assert_eq!(find_crlf2(b"HTTP/1.1 200 OK\r\nA: b\r\n\r\nxy"), Some(25));
        assert_eq!(find_crlf2(b"HTTP/1.1 200 OK\r\nA: b\r\n"), None);
        assert_eq!(find_crlf2(b"\r\n\r\n"), Some(4));
    }

    #[test]
    fn reads_the_length_and_status() {
        let (cl, ch, ok) = head_info(b"HTTP/1.1 200 OK\r\ncontent-length: 123\r\nx: y\r\n\r\n");
        assert_eq!((cl, ch, ok), (123, false, true));
        let (_, ch, ok) = head_info(b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n");
        assert!(ch && !ok);
    }

    #[test]
    fn skips_chunks() {
        assert_eq!(skip_chunked(b"3\r\nabc\r\n0\r\n\r\nrest"), Some(13));
        assert_eq!(skip_chunked(b"3\r\nabc\r\n0\r\n"), None);
    }
}
