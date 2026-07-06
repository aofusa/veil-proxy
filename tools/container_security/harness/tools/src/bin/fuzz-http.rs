//! HTTP/1.1 プロトコル境界の簡易ミューテーションファジング（コンテナ内完結・std のみ）。
//!
//! 旧 `fuzz_http.py` の Rust 置換。シードコーパスを起点に決定論的 PRNG でバイト列を
//! 変異させ、稼働中の Veil へ生 TCP で送りつける。全反復後に Veil が HTTP 応答を
//! 返せる（クラッシュ・ハングしていない）ことを health で確認する。
//!
//! 環境変数:
//!   VEIL_HOST(=veil-proxy) / VEIL_HTTP_PORT(=80) / VEIL_HTTPS_PORT(=443)
//!   FUZZ_ITERATIONS(=200)
//! 出力: /results/fuzz_report.txt / 応答不能なら exit 1。

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

/// シードコーパス（最小再現用）。旧 Python 版と同一。
fn builtin_seeds() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = vec![
        b"GET / HTTP/1.1\r\nHost: test\r\n\r\n".to_vec(),
        b"GET / HTTP/1.0\r\n\r\n".to_vec(),
        b"GET /../../../etc/passwd HTTP/1.1\r\nHost: test\r\n\r\n".to_vec(),
        b"GET /%00 HTTP/1.1\r\nHost: test\r\n\r\n".to_vec(),
        b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 9999\r\n\r\n".to_vec(),
    ];
    // 巨大ヘッダ（Host: A*8192）
    let mut big = b"GET / HTTP/1.1\r\nHost: ".to_vec();
    big.resize(big.len() + 8192, b'A');
    big.extend_from_slice(b"\r\n\r\n");
    v.push(big);
    v.push(vec![0x00, 0x01, 0x02, 0xff, 0xfe]);
    v.push(b"GET / HTTP/9.9\r\nHost: test\r\n\r\n".to_vec());
    v.push(b"HEAD / HTTP/1.1\r\nHost: test\r\n\r\n".to_vec());
    v.push(b"OPTIONS * HTTP/1.1\r\nHost: test\r\n\r\n".to_vec());
    v.push(b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nHost: test\r\n\r\n0\r\n\r\n".to_vec());
    v.push(b"GET / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n".to_vec());
    v.push(b"GET / HTTP/1.1\r\nHost: test\r\nRange: bytes=0-1024\r\n\r\n".to_vec());
    v.push(b"POST / HTTP/1.1\r\nHost: test\r\nExpect: 100-continue\r\nContent-Length: 4\r\n\r\nbody".to_vec());
    v.push(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: keep-alive\r\n".to_vec());
    v.push(Vec::new());
    v
}

/// 決定論的 PRNG（xorshift64*）。旧 Python の `random.Random(42)` に相当する固定シード。
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Rng {
            state: if seed == 0 { 0x9e3779b97f4a7c15 } else { seed },
        }
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    /// [0, n) の一様乱数（n>0）。
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    /// [lo, hi] 閉区間。
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        if hi <= lo {
            return lo;
        }
        lo + self.below(hi - lo + 1)
    }
    fn byte(&mut self) -> u8 {
        (self.next_u64() & 0xff) as u8
    }
}

/// シードをランダムに変異させる（4 種のオペレーション: bit-flip / insert / delete / append）。
fn mutate(data: &[u8], rng: &mut Rng) -> Vec<u8> {
    if data.is_empty() {
        let len = rng.range(1, 64);
        return (0..len).map(|_| rng.byte()).collect();
    }
    let mut buf = data.to_vec();
    match rng.below(4) {
        0 => {
            let idx = rng.below(buf.len());
            buf[idx] = rng.byte();
        }
        1 => {
            let pos = rng.range(0, buf.len());
            let insert_len = rng.range(1, 32);
            let ins: Vec<u8> = (0..insert_len).map(|_| rng.byte()).collect();
            buf.splice(pos..pos, ins);
        }
        2 => {
            let start = rng.below(buf.len());
            let end = (start + rng.range(1, buf.len() - start)).min(buf.len());
            buf.drain(start..end);
        }
        _ => {
            let append_len = rng.range(1, 128);
            buf.extend((0..append_len).map(|_| rng.byte()));
        }
    }
    buf
}

fn resolve(host: &str, port: u16) -> Option<std::net::SocketAddr> {
    (host, port).to_socket_addrs().ok()?.next()
}

/// 生バイト列を送って少量読み戻す。トランスポート成否と失敗理由を返す。
fn send_raw(host: &str, port: u16, payload: &[u8], timeout: Duration) -> (bool, String) {
    let addr = match resolve(host, port) {
        Some(a) => a,
        None => return (false, "resolve failed".to_string()),
    };
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(mut sock) => {
            let _ = sock.set_read_timeout(Some(timeout));
            let _ = sock.set_write_timeout(Some(timeout));
            if !payload.is_empty() {
                if let Err(e) = sock.write_all(payload) {
                    return (false, e.to_string());
                }
            }
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            (true, "ok".to_string())
        }
        Err(e) => (false, e.to_string()),
    }
}

fn verify_health(host: &str, port: u16) -> bool {
    let addr = match resolve(host, port) {
        Some(a) => a,
        None => return false,
    };
    match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        Ok(mut sock) => {
            let _ = sock.set_read_timeout(Some(Duration::from_secs(3)));
            if sock
                .write_all(b"GET / HTTP/1.1\r\nHost: health\r\nConnection: close\r\n\r\n")
                .is_err()
            {
                return false;
            }
            let mut buf = [0u8; 4096];
            match sock.read(&mut buf) {
                Ok(n) => buf[..n].windows(5).any(|w| w == b"HTTP/"),
                Err(_) => false,
            }
        }
        Err(_) => false,
    }
}

fn main() {
    let host = env::var("VEIL_HOST").unwrap_or_else(|_| "veil-proxy".to_string());
    let http_port: u16 = env::var("VEIL_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(80);
    let https_port: u16 = env::var("VEIL_HTTPS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(443);
    let iterations: usize = env::var("FUZZ_ITERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    let mut seeds = builtin_seeds();
    // /fixtures/seeds 配下のファイルも追加コーパスとして取り込む。
    let fixtures = Path::new("/fixtures/seeds");
    if fixtures.is_dir() {
        if let Ok(entries) = fs::read_dir(fixtures) {
            let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                if p.is_file() {
                    if let Ok(bytes) = fs::read(&p) {
                        seeds.push(bytes);
                    }
                }
            }
        }
    }

    let mut rng = Rng::new(42);
    let timeout = Duration::from_secs(2);
    let mut sent = 0usize;
    let mut errors = 0usize;
    let mut lines: Vec<String> = Vec::new();

    for i in 0..iterations {
        let seed = &seeds[i % seeds.len()];
        let payload = if i < seeds.len() {
            seed.clone()
        } else {
            mutate(seed, &mut rng)
        };
        let (ok, msg) = send_raw(&host, http_port, &payload, timeout);
        sent += 1;
        if !ok {
            errors += 1;
            if errors <= 20 {
                lines.push(format!("iter={i} error={msg}"));
            }
        }
        // HTTPS 境界にも少数送信。
        if i % 10 == 0 {
            let _ = send_raw(&host, https_port, &payload, timeout);
        }
    }

    std::thread::sleep(Duration::from_millis(500));
    let healthy = verify_health(&host, http_port);
    lines.push(format!(
        "sent={sent} transport_errors={errors} healthy_after_fuzz={healthy}"
    ));

    let report = "/results/fuzz_report.txt";
    if let Some(parent) = Path::new(report).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let body = lines.join("\n") + "\n";
    let _ = fs::write(report, &body);
    print!("{body}");

    if !healthy {
        eprintln!("fuzz: veil がファジング後に応答不能");
        std::process::exit(1);
    }
    println!("fuzz: ok");
}
