//! B-497: 多数の接続が一度に来ても、忙しいワーカーの受理が遅れないことの回帰試験。
//!
//! io_uring の `Accept` は実行器の 1 周回に 1 件しか終わらない。他の接続の処理で 1 周回が長いワーカーへ多数の接続が
//! 一度に来ると（接続の集約〈F-303〉が少数のワーカーへ寄せたとき等）、最後の接続の最初の応答が数秒遅れていた
//! （HTTP/2 の 1024 接続の計測で、最初の応答まで約 5 秒）。今は `Accept` の後に溜まった分を同じ周回で受理する
//! （`accept_batch`。reactor は前から同じ）。ワーカー 1 本を HTTP/1.1 のパイプラインで忙しくした上で多数の接続を
//! 一度に張り、全部の最初の応答が届くまでの時間を見る。

// 理由付き allow: テストのクライアントは同期 I/O・sleep を意図的に使う（データプレーンとは別プロセス）。
#![allow(clippy::disallowed_methods)]

use super::standalone::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const REQ: &[u8] = b"GET /fixed HTTP/1.1\r\nHost: t\r\n\r\n";
/// ワーカーを忙しくするクライアントのスレッド数・スレッドごとの接続数・接続ごとにまとめて送る要求の数
const BUSY_THREADS: usize = 3;
const BUSY_CONNS: usize = 16;
const BUSY_PIPELINE: usize = 16;
/// 一度に張る接続の数
const BURST: usize = 300;
/// 全部の最初の応答が届くまでの上限（忙しい負荷の 1 往復〈ワーカーの 1〜2 周回〉の何倍か）。1 周回に 1 件ずつ
/// 受理すると約 `BURST` 周回（往復の 100 倍を超える）、32 件ずつなら 10 周回ほどで終わる。機械の速さ・ビルドの
/// 種類によらないように往復の時間を測って決める
const BURST_LIMIT_ROUNDS: u32 = 40;
/// 上限の下限（往復が短いリリースビルドでの揺れの分）
const BURST_LIMIT_FLOOR: Duration = Duration::from_millis(500);

/// 固定応答のルートだけを平文の HTTP/1.1 で受ける、ワーカー 1 本の veil を起動する。戻り値は（プロセス, 平文のポート）
fn spawn_one_worker(dir: &Path) -> (VeilProcess, u16) {
    let (cert, key) = write_self_signed(dir);
    let port = free_port();
    let tls_port = free_port();
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{tls_port}"
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
threads = 1
[logging]
level = "warn"
[tls]
cert_path = "{cert}"
key_path = "{key}"
ktls_enabled = false

[[route]]
[route.conditions]
path = "/fixed"
[route.action]
type = "Respond"
status = 200
body = "f"
"#,
        cert = tp(&cert),
        key = tp(&key),
    );
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );
    (veil, port)
}

/// 1 つの応答の長さ（ヘッダと本文）。応答はどれも同じ長さ（`Date` は固定幅）
fn response_len(port: u16) -> usize {
    let mut s = connect_local(port, Duration::from_secs(5)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(REQ).unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = s.read(&mut tmp).unwrap();
        assert!(n > 0, "closed before the response");
        buf.extend_from_slice(&tmp[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let cl: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .expect("content-length");
            return end + 4 + cl;
        }
    }
}

/// `BUSY_CONNS` 本の接続で、`BUSY_PIPELINE` 本ずつの要求を送っては応答を全部読む、を `stop` まで続ける
fn busy(port: u16, len: usize, stop: Arc<AtomicBool>, served: Arc<AtomicU64>) {
    let mut conns: Vec<TcpStream> = (0..BUSY_CONNS)
        .map(|_| {
            let s = connect_local(port, Duration::from_secs(5)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            s
        })
        .collect();
    let batch = REQ.repeat(BUSY_PIPELINE);
    let mut buf = vec![0u8; 64 * 1024];
    while !stop.load(Ordering::Relaxed) {
        for c in &mut conns {
            c.write_all(&batch).unwrap();
        }
        for c in &mut conns {
            let mut need = len * BUSY_PIPELINE;
            while need > 0 {
                let n = c.read(&mut buf[..need.min(64 * 1024)]).unwrap();
                assert!(n > 0, "busy connection closed");
                need -= n;
            }
        }
        served.fetch_add((BUSY_CONNS * BUSY_PIPELINE) as u64, Ordering::Relaxed);
    }
}

#[test]
fn b497_burst_of_connections_is_accepted_while_busy() {
    let dir = tempfile::tempdir().unwrap();
    let (veil, port) = spawn_one_worker(dir.path());
    let len = response_len(port);

    let stop = Arc::new(AtomicBool::new(false));
    let served = Arc::new(AtomicU64::new(0));
    let busy_threads: Vec<_> = (0..BUSY_THREADS)
        .map(|_| {
            let (stop, served) = (stop.clone(), served.clone());
            std::thread::spawn(move || busy(port, len, stop, served))
        })
        .collect();
    // ワーカーが忙しくなるまで待つ
    let per_round = (BUSY_THREADS * BUSY_CONNS * BUSY_PIPELINE) as u64;
    assert!(
        wait_until(Duration::from_secs(20), || served.load(Ordering::Relaxed)
            >= 10 * per_round),
        "the busy load did not start: {}",
        veil.log()
    );
    // 忙しい負荷の 1 往復（各スレッドが全部の接続へ送って応答を全部読むまで）の時間を測る
    let (n0, t0) = (served.load(Ordering::Relaxed), Instant::now());
    std::thread::sleep(Duration::from_millis(500));
    let (n1, elapsed) = (served.load(Ordering::Relaxed), t0.elapsed());
    let round = elapsed.mul_f64(per_round as f64 / (n1 - n0).max(1) as f64);
    let limit = (round * BURST_LIMIT_ROUNDS).max(BURST_LIMIT_FLOOR);

    // 多数の接続を一度に張る（カーネルが受理待ちの列へ置くので、veil が受理する前に connect は終わる）
    let start = Instant::now();
    let mut burst: Vec<TcpStream> = (0..BURST)
        .map(|_| connect_local(port, Duration::from_secs(5)).unwrap())
        .collect();
    for s in &mut burst {
        s.write_all(REQ).unwrap();
        s.set_nonblocking(true).unwrap();
    }
    let mut got = vec![0usize; BURST];
    let mut done_at: Vec<Option<Duration>> = vec![None; BURST];
    let mut tmp = [0u8; 4096];
    let deadline = start + Duration::from_secs(30);
    while done_at.iter().any(Option::is_none) && Instant::now() < deadline {
        for (i, s) in burst.iter_mut().enumerate() {
            if done_at[i].is_some() {
                continue;
            }
            match s.read(&mut tmp) {
                Ok(0) => panic!("burst connection {i} closed before the response"),
                Ok(n) => {
                    got[i] += n;
                    if got[i] >= len {
                        done_at[i] = Some(start.elapsed());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("burst connection {i}: {e}"),
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, Ordering::Relaxed);
    for t in busy_threads {
        t.join().unwrap();
    }

    let answered = done_at.iter().filter(|d| d.is_some()).count();
    let slowest = done_at.iter().flatten().max().copied().unwrap_or_default();
    eprintln!("b497: {answered}/{BURST} answered, slowest {slowest:?}, busy round {round:?}, limit {limit:?}");
    assert_eq!(
        answered, BURST,
        "only {answered}/{BURST} burst connections were answered within 30 s"
    );
    assert!(
        slowest < limit,
        "the last of {BURST} new connections was answered after {slowest:?} (limit {limit:?}, \
         {BURST_LIMIT_ROUNDS} rounds of the busy load); a busy worker accepts too few connections per turn"
    );
}
