//! # h3load — ポータブル HTTP/3 (QUIC) 負荷生成ツール
//!
//! `tools/perf/` は HTTP/3 計測に QUIC 対応ビルドの `h2load`（Docker イメージのみ提供）を
//! 使っているが、FreeBSD 等の非 Docker プラットフォームには QUIC 対応の負荷生成クライアントが
//! パッケージとして存在しない（FreeBSD の `nghttp2` pkg の `h2load` は ngtcp2 非同梱、
//! `curl` pkg も HTTP/3 非対応）。veil は既に HTTP/3 サーバー実装で `quiche` に依存しているため、
//! 同じ crate を使った軽量クライアントを同梱し、`h2load` 互換の CLI・出力書式で置き換え可能にする。
//!
//! ## 使い方
//!
//! ```text
//! h3load [-c CONNECTIONS] [-m MAX_CONCURRENT_STREAMS] [-n TOTAL_REQUESTS] [-t THREADS] [-d SECONDS] URL
//! ```
//!
//! - `-c`（既定 1）: QUIC コネクション数。
//! - `-m`（既定 1）: コネクションあたりの最大同時リクエスト数。
//! - `-n`: 総リクエスト数（`-d` と排他）。両方省略時は `-n 1000` 相当。
//! - `-d`: 秒数で指定した時間だけ実行（固定リクエスト数の代わり）。
//! - `-t`（既定 1）: OS スレッド数。コネクションはスレッド間でできるだけ均等に分配する。
//! - `URL`: `https://host:port/path` 形式のみ対応。
//!
//! ## 重要な注意
//!
//! **証明書検証は常に無効化する**（`quiche::Config::verify_peer(false)`）。本ツールは
//! 自己署名証明書を使うベンチマーク専用であり、本番トラフィックには絶対に使わないこと。
//!
//! ## 実装方針
//!
//! これはテスト/ベンチツールであり、`AGENTS.md` のホットパス絶対規則（非同期 I/O 必須・
//! アロケーション禁止等）はデータプレーンにのみ適用され、本ファイルには適用しない。
//! veil 本体のランタイム（`src/runtime/`）・tokio 等の非同期ランタイムには依存せず、
//! `std::net::UdpSocket`（読み取りタイムアウト付き）とふつうのブロッキングループのみで
//! 構成する。コネクションごとに専用の `UdpSocket` を持たせることで、データグラムを
//! どのコネクションへ配送すべきかの判定（コネクション ID によるデマルチプレクス）が
//! 一切不要になり、実装が単純かつ明らかに正しくなる。

// ベンチ/負荷生成専用ツール（データプレーンではない）: 同期 I/O・sleep 相当のブロッキング
// 待機（recv タイムアウトによるポーリング）を意図的に使用する。AGENTS.md のホットパス
// 絶対規則はデータプレーンにのみ適用され、本ファイルは対象外。
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quiche::h3::NameValue;

/// 1 周まわして進捗が無かったときの休止時間（`idle_backoff`）。
const IDLE_BACKOFF: Duration = Duration::from_micros(50);
/// QUIC ハンドシェイクに許容する最大時間。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// アイドル（送信すべきものが無い）コネクションが完全にクローズされるまでの上限時間。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// コマンドライン引数。
struct Args {
    connections: usize,
    max_concurrent: usize,
    total_requests: Option<u64>,
    duration: Option<Duration>,
    threads: usize,
    url: String,
}

fn print_help() {
    eprintln!(
        "h3load [-c CONNECTIONS] [-m MAX_CONCURRENT_STREAMS] [-n TOTAL_REQUESTS] [-t THREADS] [-d SECONDS] URL\n\
         \n\
         ポータブル HTTP/3 (QUIC) 負荷生成ツール（h2load 互換の出力書式）。\n\
         \n\
         オプション:\n\
         \x20 -c N   QUIC コネクション数（既定 1）\n\
         \x20 -m N   コネクションあたりの最大同時リクエスト数（既定 1）\n\
         \x20 -n N   総リクエスト数（-d と排他。両方省略時は -n 1000）\n\
         \x20 -d SEC 秒数で指定した時間だけ実行（-n と排他）\n\
         \x20 -t N   OS スレッド数（既定 1。コネクションはスレッド間で均等分配）\n\
         \x20 URL    https://host:port/path のみ対応\n\
         \n\
         注意: 証明書検証は常に無効化する（verify_peer(false)）。自己署名証明書を使う\n\
         ベンチマーク専用ツールであり、本番トラフィックには使わないこと。"
    );
}

fn parse_args() -> Result<Args, String> {
    let mut connections = 1usize;
    let mut max_concurrent = 1usize;
    let mut total_requests: Option<u64> = None;
    let mut duration: Option<Duration> = None;
    let mut threads = 1usize;
    let mut url: Option<String> = None;

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        let arg = raw[i].as_str();
        let mut next_val = || -> Result<String, String> {
            i += 1;
            raw.get(i)
                .cloned()
                .ok_or_else(|| format!("{arg}: 値が指定されていない"))
        };
        match arg {
            "-c" => {
                connections = next_val()?
                    .parse()
                    .map_err(|_| "-c: 整数値が必要".to_string())?
            }
            "-m" => {
                max_concurrent = next_val()?
                    .parse()
                    .map_err(|_| "-m: 整数値が必要".to_string())?
            }
            "-n" => {
                total_requests = Some(
                    next_val()?
                        .parse()
                        .map_err(|_| "-n: 整数値が必要".to_string())?,
                )
            }
            "-d" => {
                let secs: f64 = next_val()?
                    .parse()
                    .map_err(|_| "-d: 数値が必要".to_string())?;
                duration = Some(Duration::from_secs_f64(secs))
            }
            "-t" => {
                threads = next_val()?
                    .parse()
                    .map_err(|_| "-t: 整数値が必要".to_string())?
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other if !other.starts_with('-') => {
                if url.is_some() {
                    return Err(format!("URL は 1 つのみ指定可能: {other}"));
                }
                url = Some(other.to_string());
            }
            other => return Err(format!("不明なオプション: {other}")),
        }
        i += 1;
    }

    if total_requests.is_some() && duration.is_some() {
        return Err("-n と -d は同時に指定できない".to_string());
    }
    if total_requests.is_none() && duration.is_none() {
        total_requests = Some(1000);
    }
    if connections == 0 || max_concurrent == 0 || threads == 0 {
        return Err("-c / -m / -t は 1 以上を指定すること".to_string());
    }

    let url = url.ok_or_else(|| "URL が指定されていない".to_string())?;

    Ok(Args {
        connections,
        max_concurrent,
        total_requests,
        duration,
        threads,
        url,
    })
}

/// `https://host:port/path` 形式の URL を分解する（HTTP/3 専用のため https のみ受け付ける）。
fn parse_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("URL は https:// で始まる必要がある: {url}"))?;

    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], rest[idx..].to_string()),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err(format!("URL にホストが無い: {url}"));
    }

    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("ポート番号が不正: {authority}"))?,
        ),
        None => (authority.to_string(), 443u16),
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path
    };

    Ok((host, port, path))
}

/// 全リクエストの発行可否を管理する予算。`-n`（総数） / `-d`（時間）のいずれか片方が有効。
enum Budget {
    Count(Arc<AtomicU64>),
    Time(Instant),
}

impl Budget {
    /// 新規リクエストを 1 件発行してよいか判定する（`Count` は成功時に内部カウンタを消費する）。
    fn try_acquire(&self) -> bool {
        match self {
            Budget::Count(remaining) => remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
                .is_ok(),
            Budget::Time(deadline) => Instant::now() < *deadline,
        }
    }
}

/// スレッドをまたいで集計する統計情報。
#[derive(Default, Clone)]
struct Stats {
    total: u64,
    started: u64,
    done: u64,
    succeeded: u64,
    failed: u64,
    errored: u64,
    status_2xx: u64,
    status_3xx: u64,
    status_4xx: u64,
    status_5xx: u64,
    bytes: u64,
    latencies: Vec<Duration>,
}

impl Stats {
    fn merge(&mut self, other: Stats) {
        self.total += other.total;
        self.started += other.started;
        self.done += other.done;
        self.succeeded += other.succeeded;
        self.failed += other.failed;
        self.errored += other.errored;
        self.status_2xx += other.status_2xx;
        self.status_3xx += other.status_3xx;
        self.status_4xx += other.status_4xx;
        self.status_5xx += other.status_5xx;
        self.bytes += other.bytes;
        self.latencies.extend(other.latencies);
    }
}

/// 進行中のリクエスト 1 件の状態（応答開始からストリーム完了までを追跡する）。
struct InFlight {
    start: Instant,
    status: Option<u16>,
    body_len: u64,
}

/// 1 本の QUIC コネクションとその上で多重化されるリクエストを管理する。
struct ConnCtx {
    socket: UdpSocket,
    peer_addr: SocketAddr,
    conn: Box<quiche::Connection>,
    h3: Option<quiche::h3::Connection>,
    in_flight: HashMap<u64, InFlight>,
    /// 発行済みだが予算切れ・時間切れのため、これ以上新規発行しない状態か。
    no_more_requests: bool,
    /// クローズ処理に入った時刻（アイドルクローズのタイムアウト判定用）。
    closing_since: Option<Instant>,
    finished: bool,
}

impl ConnCtx {
    fn connect(host: &str, peer_addr: SocketAddr) -> Result<Self, String> {
        let mut config =
            quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| e.to_string())?;
        config
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
            .map_err(|e| e.to_string())?;
        // 常に証明書検証を無効化する（自己署名証明書を使うベンチマーク専用ツールのため）。
        config.verify_peer(false);
        config.set_max_idle_timeout(30_000);
        config.set_max_recv_udp_payload_size(1350);
        config.set_max_send_udp_payload_size(1350);
        config.set_initial_max_data(10_000_000);
        config.set_initial_max_stream_data_bidi_local(1_000_000);
        config.set_initial_max_stream_data_bidi_remote(1_000_000);
        config.set_initial_max_stream_data_uni(1_000_000);
        config.set_initial_max_streams_bidi(256);
        config.set_initial_max_streams_uni(256);
        config.set_disable_active_migration(true);

        let bind_addr: SocketAddr = if peer_addr.is_ipv6() {
            "[::]:0".parse().unwrap()
        } else {
            "0.0.0.0:0".parse().unwrap()
        };
        let socket = UdpSocket::bind(bind_addr).map_err(|e| e.to_string())?;
        // 非ブロッキングにする。複数コネクションを 1 スレッドでラウンドロビン駆動するため、
        // 1 コネクションごとにブロッキング待ちをすると「コネクション数 × タイムアウト」の
        // 遅延がそのままレイテンシとスループットの上限になる（読み取りタイムアウト 2ms・
        // 64 コネクションで 1 周 128ms）。データが無ければ即 EWOULDBLOCK で返し、
        // 1 周まるごと進捗が無かったときだけ呼び出し側で短くスリープする。
        socket.set_nonblocking(true).map_err(|e| e.to_string())?;
        socket.connect(peer_addr).map_err(|e| e.to_string())?;
        let local_addr = socket.local_addr().map_err(|e| e.to_string())?;

        // scid はコネクションごとに専用ソケットを使う本ツールでは、ローカルでの一意性さえ
        // あれば十分（quiche 側のルーティング用でありセキュリティ用途ではないため、
        // 暗号論的乱数である必要はない）。時刻とローカルアドレスから簡易に生成する。
        let mut scid_bytes = [0u8; quiche::MAX_CONN_ID_LEN];
        let nanos = Instant::now().elapsed().as_nanos() as u64;
        let seed = nanos ^ (local_addr.port() as u64) << 16 ^ (std::process::id() as u64) << 32;
        for (idx, b) in scid_bytes.iter_mut().enumerate() {
            *b = (seed.wrapping_mul(2654435761).wrapping_add(idx as u64) >> 8) as u8;
        }
        let scid = quiche::ConnectionId::from_ref(&scid_bytes);

        let conn = quiche::connect(Some(host), &scid, local_addr, peer_addr, &mut config)
            .map_err(|e| e.to_string())?;

        Ok(ConnCtx {
            socket,
            peer_addr,
            conn: Box::new(conn),
            h3: None,
            in_flight: HashMap::new(),
            no_more_requests: false,
            closing_since: None,
            finished: false,
        })
    }

    /// 未送信の QUIC パケットをすべて送出する。
    fn flush_send(&mut self) {
        let mut out = [0u8; 1350];
        loop {
            match self.conn.send(&mut out) {
                Ok((len, _)) => {
                    if self.socket.send(&out[..len]).is_err() {
                        break;
                    }
                }
                Err(quiche::Error::Done) => break,
                Err(_) => break,
            }
        }
    }

    /// 到着している UDP データグラムを**すべて**読み取ってコネクションへ供給する。
    ///
    /// 1 件だけ読むと、1 周あたり 1 データグラムしか消化できずソケットバッファが溢れる
    /// （高並行時は取りこぼしと再送でスループットが出ない）。`WouldBlock` になるまで
    /// 読み切る。1 件でも読めたら `true` を返す（呼び出し側の進捗判定に使う）。
    fn recv_drain(&mut self) -> bool {
        let mut buf = [0u8; 65535];
        let local_addr = match self.socket.local_addr() {
            Ok(a) => a,
            Err(_) => return false,
        };
        let recv_info = quiche::RecvInfo {
            from: self.peer_addr,
            to: local_addr,
        };
        let mut progressed = false;
        loop {
            match self.socket.recv(&mut buf) {
                Ok(len) => {
                    progressed = true;
                    let _ = self.conn.recv(&mut buf[..len], recv_info);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                    return progressed
                }
                Err(_) => return progressed,
            }
        }
    }

    /// 期限の来た QUIC タイマーを発火させる。
    ///
    /// **これを呼ばないとハンドシェイクが必ずデッドロックする**（実測）。quiche は ACK を
    /// 遅延させる（`max_ack_delay`）ため、クライアントはサーバの Initial/Handshake を
    /// 受け取っても即座には ACK を返さず、ACK タイマーの発火を待つ。一方サーバは
    /// アドレス検証前の増幅制限（受信バイト数の 3 倍まで）に達しており、ACK を受け取る
    /// までハンドシェイクの続きを送れない。`on_timeout()` を呼ばないとこの ACK が永久に
    /// 送られず、双方が相手を待ったまま停止する（クライアントは 1200 バイト × 2 を送って
    /// 同量を受け取ったところで無音になる）。
    ///
    /// `Connection::timeout()` は次のタイマーまでの残り時間を返し、期限到来時は
    /// ゼロ幅を返すため、それを発火条件にする。ロス検知の再送タイマーも同じ経路で処理される。
    fn drive_timers(&mut self) {
        if self.conn.timeout().is_some_and(|d| d.is_zero()) {
            self.conn.on_timeout();
        }
    }

    /// HTTP/3 層が確立していなければ試みる。
    fn ensure_h3(&mut self) {
        if self.h3.is_some() || !self.conn.is_established() {
            return;
        }
        let h3_config = match quiche::h3::Config::new() {
            Ok(c) => c,
            Err(_) => return,
        };
        if let Ok(h3_conn) = quiche::h3::Connection::with_transport(&mut self.conn, &h3_config) {
            self.h3 = Some(h3_conn);
        }
    }

    /// 予算が許す限り、`max_concurrent` に達するまで新規リクエストを発行する。
    fn issue_requests(
        &mut self,
        path: &str,
        authority: &str,
        max_concurrent: usize,
        budget: &Budget,
        stats: &mut Stats,
    ) {
        let Some(h3) = self.h3.as_mut() else {
            return;
        };
        while self.in_flight.len() < max_concurrent {
            if !budget.try_acquire() {
                self.no_more_requests = true;
                break;
            }
            let headers = vec![
                quiche::h3::Header::new(b":method", b"GET"),
                quiche::h3::Header::new(b":path", path.as_bytes()),
                quiche::h3::Header::new(b":authority", authority.as_bytes()),
                quiche::h3::Header::new(b":scheme", b"https"),
            ];
            match h3.send_request(&mut self.conn, &headers, true) {
                Ok(stream_id) => {
                    stats.started += 1;
                    self.in_flight.insert(
                        stream_id,
                        InFlight {
                            start: Instant::now(),
                            status: None,
                            body_len: 0,
                        },
                    );
                }
                Err(_) => {
                    // 送信できなかった分は消費した予算とともに失敗扱いにする
                    // （h2load 的には「開始できなかった」扱いに相当する）。
                    stats.errored += 1;
                    break;
                }
            }
        }
    }

    /// 受信済みデータに対する HTTP/3 イベントを処理し、統計へ反映する。
    ///
    /// 1 件でもイベントを処理したら `true` を返す（呼び出し側の進捗判定に使う）。
    fn poll_h3_events(&mut self, stats: &mut Stats) -> bool {
        let mut buf = [0u8; 65535];
        let mut progressed = false;
        while let Some(h3) = self.h3.as_mut() {
            match h3.poll(&mut self.conn) {
                Ok((stream_id, quiche::h3::Event::Headers { list, .. })) => {
                    if let Some(req) = self.in_flight.get_mut(&stream_id) {
                        if let Some(status_hdr) = list.iter().find(|h| h.name() == b":status") {
                            req.status = std::str::from_utf8(status_hdr.value())
                                .ok()
                                .and_then(|s| s.parse::<u16>().ok());
                        }
                    }
                }
                Ok((stream_id, quiche::h3::Event::Data)) => {
                    if let Some(h3) = self.h3.as_mut() {
                        while let Ok(read) = h3.recv_body(&mut self.conn, stream_id, &mut buf) {
                            if let Some(req) = self.in_flight.get_mut(&stream_id) {
                                req.body_len += read as u64;
                            }
                            if read == 0 {
                                break;
                            }
                        }
                    }
                }
                Ok((stream_id, quiche::h3::Event::Finished)) => {
                    if let Some(req) = self.in_flight.remove(&stream_id) {
                        finish_request(stats, req);
                    }
                }
                Ok((stream_id, quiche::h3::Event::Reset(_))) => {
                    if let Some(req) = self.in_flight.remove(&stream_id) {
                        stats.done += 1;
                        stats.failed += 1;
                        let _ = req;
                    }
                }
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(_) => break,
            }
            progressed = true;
        }
        progressed
    }

    /// このコネクションでもうやることが無いか判定し、無ければクローズを試みる。
    fn maybe_finish(&mut self) {
        if self.finished {
            return;
        }
        if self.conn.is_closed() {
            self.finished = true;
            return;
        }
        if self.no_more_requests && self.in_flight.is_empty() {
            match self.closing_since {
                None => {
                    self.closing_since = Some(Instant::now());
                    let _ = self.conn.close(true, 0x00, b"done");
                    self.flush_send();
                }
                Some(since) if since.elapsed() > CLOSE_TIMEOUT => {
                    self.finished = true;
                }
                Some(_) => {}
            }
        }
    }
}

/// ストリーム完了時に統計へ反映する共通処理。
fn finish_request(stats: &mut Stats, req: InFlight) {
    stats.done += 1;
    stats.bytes += req.body_len;
    let elapsed = req.start.elapsed();
    match req.status {
        Some(code) if (200..300).contains(&code) => {
            stats.succeeded += 1;
            stats.status_2xx += 1;
            stats.latencies.push(elapsed);
        }
        Some(code) if (300..400).contains(&code) => {
            stats.succeeded += 1;
            stats.status_3xx += 1;
            stats.latencies.push(elapsed);
        }
        Some(code) if (400..500).contains(&code) => {
            stats.failed += 1;
            stats.status_4xx += 1;
        }
        Some(code) if (500..600).contains(&code) => {
            stats.failed += 1;
            stats.status_5xx += 1;
        }
        _ => {
            stats.errored += 1;
        }
    }
}

/// 1 スレッド分のコネクション群を確立し、リクエストが尽きるまで駆動する。
#[allow(clippy::too_many_arguments)]
/// 1 周まわして全コネクションに何の進捗も無かったときの待機。
///
/// 非ブロッキング I/O にした結果、何も起きていないときはビジーループになる。負荷生成側が
/// 計測対象と CPU を奪い合うと計測が歪むため、ごく短い休止を入れて CPU を明け渡す。
/// 十分短いのでレイテンシ計測への影響は無視できる。
fn idle_backoff() {
    std::thread::sleep(IDLE_BACKOFF);
}

fn run_thread(
    host: String,
    peer_addr: SocketAddr,
    path: String,
    authority: String,
    n_conns: usize,
    max_concurrent: usize,
    budget: Arc<Budget>,
) -> Stats {
    let mut stats = Stats::default();
    let mut conns: Vec<ConnCtx> = Vec::with_capacity(n_conns);
    for _ in 0..n_conns {
        match ConnCtx::connect(&host, peer_addr) {
            Ok(mut c) => {
                c.flush_send();
                conns.push(c);
            }
            Err(e) => {
                eprintln!("h3load: コネクション確立に失敗: {e}");
            }
        }
    }

    // ハンドシェイク完了を待つ（コネクションごとに独立、ラウンドロビンでポーリング）。
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let all_settled = conns
            .iter()
            .all(|c| c.conn.is_established() || c.conn.is_closed());
        if all_settled || Instant::now() > handshake_deadline {
            break;
        }
        let mut progressed = false;
        for c in conns.iter_mut() {
            if c.conn.is_established() || c.conn.is_closed() {
                continue;
            }
            progressed |= c.recv_drain();
            c.drive_timers();
            c.flush_send();
        }
        if !progressed {
            idle_backoff();
        }
    }
    conns.retain(|c| {
        let ok = c.conn.is_established();
        if !ok {
            eprintln!("h3load: QUIC ハンドシェイクに失敗またはタイムアウト");
        }
        ok
    });
    for c in conns.iter_mut() {
        c.ensure_h3();
    }

    // メインループ: 全コネクションが finished になるまでラウンドロビンで駆動する。
    while conns.iter().any(|c| !c.finished) {
        let mut progressed = false;
        for c in conns.iter_mut() {
            if c.finished {
                continue;
            }
            progressed |= c.recv_drain();
            c.drive_timers();
            progressed |= c.poll_h3_events(&mut stats);
            if !c.no_more_requests {
                c.issue_requests(&path, &authority, max_concurrent, &budget, &mut stats);
            }
            c.flush_send();
            c.maybe_finish();
        }
        if !progressed {
            idle_backoff();
        }
    }

    stats
}

/// quiche が要求する BoringSSL 互換シンボルのリンクを強制する（Linux 用）。
///
/// Linux では quiche を `default-features = false` でビルドし、BoringSSL の実体は
/// rustls 側と共有する `aws-lc-sys`（`AWS_LC_SYS_NO_PREFIX=1`）が提供する
/// （Cargo.toml のターゲット別依存を参照）。ところが example は `quiche` だけを
/// 参照していると `aws-lc-sys` のネイティブライブラリがリンク対象に入らず、
/// `TLS_method` / `SSL_set_min_proto_version` / `AES_set_encrypt_key` 等が
/// undefined symbol になってリンクに失敗する（実測）。
///
/// veil 本体の暗号プロバイダを 1 回参照することで、その依存として
/// `aws-lc-sys`（OpenBSD/NetBSD では `ring`）がリンク対象に入る。呼び出し自体に
/// 副作用は無く、計測結果にも影響しない。
fn force_crypto_backend_link() {
    let _ = veil::tls_provider::provider::default_provider();
}

fn main() {
    force_crypto_backend_link();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("h3load: {e}\n");
            print_help();
            std::process::exit(1);
        }
    };

    let (host, port, path) = match parse_url(&args.url) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("h3load: {e}");
            std::process::exit(1);
        }
    };
    let authority = if port == 443 {
        host.clone()
    } else {
        format!("{host}:{port}")
    };

    let peer_addr = match format!("{host}:{port}").to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(a) => a,
            None => {
                eprintln!("h3load: ホスト名を解決できない: {host}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("h3load: ホスト名解決エラー: {e}");
            std::process::exit(1);
        }
    };

    let budget = Arc::new(match (args.total_requests, args.duration) {
        (Some(n), None) => Budget::Count(Arc::new(AtomicU64::new(n))),
        (None, Some(d)) => Budget::Time(Instant::now() + d),
        _ => unreachable!("parse_args で -n / -d の排他は保証済み"),
    });
    let planned_total = args.total_requests;

    // コネクションをスレッド間へできるだけ均等に分配する（余りは先頭のスレッドから 1 つずつ）。
    let base = args.connections / args.threads;
    let extra = args.connections % args.threads;
    let per_thread: Vec<usize> = (0..args.threads)
        .map(|i| base + if i < extra { 1 } else { 0 })
        .filter(|&n| n > 0)
        .collect();

    let start = Instant::now();
    let mut handles = Vec::with_capacity(per_thread.len());
    for n_conns in per_thread {
        let host = host.clone();
        let path = path.clone();
        let authority = authority.clone();
        let budget = Arc::clone(&budget);
        let max_concurrent = args.max_concurrent;
        handles.push(std::thread::spawn(move || {
            run_thread(
                host,
                peer_addr,
                path,
                authority,
                n_conns,
                max_concurrent,
                budget,
            )
        }));
    }

    let mut total_stats = Stats::default();
    for h in handles {
        match h.join() {
            Ok(s) => total_stats.merge(s),
            Err(_) => eprintln!("h3load: 負荷生成スレッドが panic した"),
        }
    }
    let elapsed = start.elapsed();

    total_stats.total = match planned_total {
        Some(n) => n,
        None => total_stats.started,
    };

    print_summary(&total_stats, elapsed);
}

/// マイクロ秒/ミリ秒を h2load 風に読みやすい単位へフォーマットする。
fn format_duration(d: Duration) -> String {
    let us = d.as_secs_f64() * 1_000_000.0;
    if us < 1000.0 {
        format!("{us:.2}us")
    } else {
        format!("{:.2}ms", us / 1000.0)
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// `tools/perf/freebsd/run_perf_freebsd.sh` の `run_h3load()` が awk でパースする
/// h2load 互換の出力書式で最終結果を出力する。
fn print_summary(stats: &Stats, elapsed: Duration) {
    let secs = elapsed.as_secs_f64().max(0.000_001);
    let rps = stats.succeeded as f64 / secs;
    let bps = stats.bytes as f64 / secs;

    println!("finished in {:.2}s, {:.2} req/s, {:.2}B/s", secs, rps, bps);
    println!(
        "requests: {} total, {} started, {} done, {} succeeded, {} failed, {} errored, 0 timeout",
        stats.total, stats.started, stats.done, stats.succeeded, stats.failed, stats.errored
    );
    println!(
        "status codes: {} 2xx, {} 3xx, {} 4xx, {} 5xx",
        stats.status_2xx, stats.status_3xx, stats.status_4xx, stats.status_5xx
    );

    if stats.latencies.is_empty() {
        println!("time for request: min 0us max 0us mean 0us p50 0us p99 0us");
        return;
    }
    let mut sorted = stats.latencies.clone();
    sorted.sort();
    let min = *sorted.first().unwrap();
    let max = *sorted.last().unwrap();
    let mean_us: f64 = sorted
        .iter()
        .map(|d| d.as_secs_f64() * 1_000_000.0)
        .sum::<f64>()
        / sorted.len() as f64;
    let mean = Duration::from_secs_f64(mean_us / 1_000_000.0);
    let p50 = percentile(&sorted, 0.50);
    let p99 = percentile(&sorted, 0.99);

    println!(
        "time for request: min {} max {} mean {} p50 {} p99 {}",
        format_duration(min),
        format_duration(max),
        format_duration(mean),
        format_duration(p50),
        format_duration(p99)
    );
}
