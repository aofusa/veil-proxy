//! # h3load — ポータブル HTTP/3 (QUIC) 負荷生成ツール
//!
//! `tools/perf/` は HTTP/3 計測に QUIC 対応ビルドの `h2load`（Docker イメージのみ提供）を
//! 使っているが、FreeBSD 等の非 Docker プラットフォームには QUIC 対応の負荷生成クライアントが
//! パッケージとして存在しない（FreeBSD の `nghttp2` pkg の `h2load` は ngtcp2 非同梱、
//! `curl` pkg も HTTP/3 非対応）。そこで `h2load` 互換の CLI・出力書式のクライアントを
//! 自前で同梱し、置き換え可能にする。
//!
//! ## 依存ライブラリの選定方針（AGENTS.md 参照）
//!
//! veil は **`quiche` を本番データプレーン専用**（`src/http3_server.rs`）と位置づけ、
//! **テスト・計測ツールは `quinn` + `h3`** を使う方針にしている
//! （`tests/common/http3_client.rs` と本ツールが該当）。本ツールは perf 計測用であり
//! データプレーンではないため、後者の方針に従い `quiche` ではなく `quinn` + `h3` で
//! 実装する。接続確立・ドライバ駆動・証明書検証スキップの構成は
//! `tests/common/http3_client.rs` に倣っている。
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
//! - `-t`（既定 1）: tokio ワーカースレッド数。
//! - `URL`: `https://host:port/path` 形式のみ対応。
//!
//! ## 重要な注意
//!
//! **証明書検証は常に無効化する**（自己署名証明書向けのカスタム `ServerCertVerifier`）。
//! 本ツールは自己署名証明書を使うベンチマーク専用であり、本番トラフィックには絶対に使わないこと。
//!
//! ## タイマー処理について
//!
//! 旧 `quiche` 実装では、クライアントが `Connection::on_timeout()` を呼ばないと
//! ACK 遅延タイマーが発火せずハンドシェイクがデッドロックするという落とし穴があった
//! （quiche は自前ループでタイマーを手動駆動する必要がある）。`quinn` はコネクションの
//! タイマー（ACK 遅延・損失検知・アイドルタイムアウト等）を内部の非同期タスクとして
//! 自動的に駆動するため、このクラスのバグはそもそも発生しない。
//!
//! ## 配置とビルド（本体から独立）
//!
//! 本ツールは **veil 本体のワークスペース外の独立クレート**である
//! （ルート `Cargo.toml` の `[workspace] exclude`）。以前はルートの `[[example]]` として
//! 宣言していたが、cargo はマニフェスト解析時点で宣言済みターゲットのソース有無を
//! 検査するため、Docker のビルドコンテキスト（`.dockerignore` がホワイトリスト方式）に
//! ソースが含まれないだけで**全プラットフォームのコンテナビルドが失敗**した。
//! 計測専用ツールが配布ビルドを壊さないよう分離してある。
//!
//! ```text
//! cargo build --release --manifest-path tools/perf/h3load/Cargo.toml
//! ```
//!
//! ## 実装方針
//!
//! これはテスト/ベンチツールであり、`AGENTS.md` のホットパス絶対規則（非同期 I/O 必須・
//! アロケーション禁止等）はデータプレーンにのみ適用され、本ファイルには適用しない。
//! `quinn` + `tokio` の非同期 API を素直に使い、明快さを優先する。

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Buf;
use h3::client::SendRequest;
use http::Request;
use quinn::Endpoint;

// F-122/B-51 の本体・テストクライアント方針と同様、rustls 暗号プロバイダは ring を使う
// （aws-lc-rs を使うと本体のターゲット別 TLS 設定に相乗りすることになり、本ツールを
// 「veil 本体から独立させる」という設計方針に反する。ring は常に無条件で使える）。
use rustls::crypto::ring as tls_crypto;

/// QUIC ハンドシェイクに許容する最大時間。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
         \x20 -t N   tokio ワーカースレッド数（既定 1）\n\
         \x20 URL    https://host:port/path のみ対応\n\
         \n\
         注意: 証明書検証は常に無効化する。自己署名証明書を使う\n\
         ベンチマーク専用ツールであり、本番トラフィックには使わないこと。"
    );
}

/// 値が密着した短オプション（`-c64`）をフラグと値に分解する。
///
/// `h2load` は `-c64` と `-c 64` の**両方**を受理するため、h2load 互換を掲げる本ツールも
/// 両方を受理する必要がある。実際 `tools/perf/freebsd/run_perf_freebsd.sh` は
/// `-t2 -c64` の密着形で渡しており、密着形を弾いていたために **FreeBSD の HTTP/3 計測が
/// 丸ごと 0 rps になっていた**（2026-08-11 に発覚）。
///
/// 戻り値は `(フラグ, 密着した値)`。密着値が無ければ第 2 要素は `None`。
/// `--help` のような長オプションと、単独の `-c` は分解しない。
fn split_short_opt(arg: &str) -> (&str, Option<&str>) {
    const VALUE_OPTS: [&str; 5] = ["-c", "-m", "-n", "-d", "-t"];
    if arg.len() > 2 && arg.starts_with('-') && !arg.starts_with("--") {
        let (head, rest) = arg.split_at(2);
        if VALUE_OPTS.contains(&head) {
            return (head, Some(rest));
        }
    }
    (arg, None)
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
        let raw_arg = raw[i].clone();
        let (arg, inline) = split_short_opt(&raw_arg);
        let mut next_val = || -> Result<String, String> {
            // 密着形（`-c64`）ならその値を使い、分離形（`-c 64`）なら次の引数を読む。
            if let Some(v) = inline {
                return Ok(v.to_string());
            }
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
    Count(AtomicU64),
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

    /// レスポンスステータス・所要時間・受信バイト数から統計を更新する共通処理。
    fn record_response(&mut self, status: u16, body_len: u64, elapsed: Duration) {
        self.done += 1;
        self.bytes += body_len;
        match status {
            200..=299 => {
                self.succeeded += 1;
                self.status_2xx += 1;
                self.latencies.push(elapsed);
            }
            300..=399 => {
                self.succeeded += 1;
                self.status_3xx += 1;
                self.latencies.push(elapsed);
            }
            400..=499 => {
                self.failed += 1;
                self.status_4xx += 1;
            }
            500..=599 => {
                self.failed += 1;
                self.status_5xx += 1;
            }
            _ => {
                self.errored += 1;
            }
        }
    }
}

/// 証明書検証を常にスキップするカスタム検証器。
///
/// 本ツールは自己署名証明書を使うベンチマーク専用であり、本番トラフィックには
/// 絶対に使わないこと。`tests/common/http3_client.rs` と同じ構成。
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        tls_crypto::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
            .to_vec()
    }
}

/// 証明書検証を無効化した QUIC 用クライアントエンドポイントを作成する。
fn build_endpoint() -> Result<Endpoint, Box<dyn std::error::Error + Send + Sync>> {
    let _ = rustls::crypto::CryptoProvider::install_default(tls_crypto::default_provider());

    let mut tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"h3".to_vec()];

    let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_config));

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into()?));
    client_config.transport_config(Arc::new(transport));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

/// 1 件の GET リクエストを送信し、ステータスコードと受信バイト数を返す。
async fn send_one_request(
    send_request: &mut SendRequest<h3_quinn::OpenStreams, bytes::Bytes>,
    authority: &str,
    path: &str,
) -> Result<(u16, u64), Box<dyn std::error::Error + Send + Sync>> {
    let uri = format!("https://{authority}{path}");
    let request = Request::builder().method("GET").uri(uri).body(())?;

    let mut stream = send_request.send_request(request).await?;
    stream.finish().await?;

    let response = stream.recv_response().await?;
    let status = response.status().as_u16();

    let mut body_len = 0u64;
    while let Some(chunk) = stream.recv_data().await? {
        body_len += chunk.remaining() as u64;
    }

    Ok((status, body_len))
}

/// 1 本の QUIC コネクションを確立し、`max_concurrent` 並行度でリクエストが尽きるまで駆動する。
async fn run_connection(
    endpoint: Endpoint,
    peer_addr: SocketAddr,
    host: String,
    authority: String,
    path: String,
    max_concurrent: usize,
    budget: Arc<Budget>,
) -> Stats {
    let mut stats = Stats::default();

    let connecting = match endpoint.connect(peer_addr, &host) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("h3load: コネクション確立に失敗: {e}");
            return stats;
        }
    };
    let conn = match tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            eprintln!("h3load: QUIC ハンドシェイクに失敗: {e}");
            return stats;
        }
        Err(_) => {
            eprintln!("h3load: QUIC ハンドシェイクがタイムアウトした");
            return stats;
        }
    };

    let h3_conn = h3_quinn::Connection::new(conn.clone());
    let (mut driver, send_request) = match h3::client::new(h3_conn).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("h3load: HTTP/3 接続の確立に失敗: {e}");
            return stats;
        }
    };

    // ドライバーをバックグラウンドで駆動する（QUIC タイマー・フロー制御は quinn が内部で処理する）。
    let driver_task = tokio::spawn(async move {
        let _ = driver.wait_idle().await;
    });

    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..max_concurrent {
        let mut send_request = send_request.clone();
        let budget = Arc::clone(&budget);
        let authority = authority.clone();
        let path = path.clone();
        workers.spawn(async move {
            let mut local = Stats::default();
            while budget.try_acquire() {
                local.started += 1;
                let req_start = Instant::now();
                match send_one_request(&mut send_request, &authority, &path).await {
                    Ok((status, body_len)) => {
                        local.record_response(status, body_len, req_start.elapsed());
                    }
                    Err(_) => {
                        local.done += 1;
                        local.errored += 1;
                    }
                }
            }
            local
        });
    }
    // 全ワーカーが持つ send_request のクローンをここで手放してからでないと、次の
    // conn.close() が「まだ生きている送信ハンドルがある」状態になり得るため、
    // まず join し切ってから明示クローズする。
    drop(send_request);
    while let Some(res) = workers.join_next().await {
        match res {
            Ok(s) => stats.merge(s),
            Err(e) => eprintln!("h3load: リクエストワーカーが panic した: {e}"),
        }
    }

    conn.close(0u32.into(), b"done");
    driver_task.abort();

    stats
}

async fn run(
    args: Args,
    host: String,
    authority: String,
    path: String,
    peer_addr: SocketAddr,
) -> Stats {
    let endpoint = match build_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("h3load: QUIC エンドポイントの作成に失敗: {e}");
            std::process::exit(1);
        }
    };

    let budget = Arc::new(match (args.total_requests, args.duration) {
        (Some(n), None) => Budget::Count(AtomicU64::new(n)),
        (None, Some(d)) => Budget::Time(Instant::now() + d),
        _ => unreachable!("parse_args で -n / -d の排他は保証済み"),
    });

    let mut conns = tokio::task::JoinSet::new();
    for _ in 0..args.connections {
        let endpoint = endpoint.clone();
        let host = host.clone();
        let authority = authority.clone();
        let path = path.clone();
        let budget = Arc::clone(&budget);
        let max_concurrent = args.max_concurrent;
        conns.spawn(run_connection(
            endpoint,
            peer_addr,
            host,
            authority,
            path,
            max_concurrent,
            budget,
        ));
    }

    let mut total_stats = Stats::default();
    while let Some(res) = conns.join_next().await {
        match res {
            Ok(s) => total_stats.merge(s),
            Err(e) => eprintln!("h3load: コネクションタスクが panic した: {e}"),
        }
    }

    total_stats.total = match args.total_requests {
        Some(n) => n,
        None => total_stats.started,
    };

    total_stats
}

fn main() {
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

    let threads = args.threads;
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("h3load: tokio ランタイムの構築に失敗: {e}");
            std::process::exit(1);
        }
    };

    let start = Instant::now();
    let stats = rt.block_on(run(args, host, authority, path, peer_addr));
    let elapsed = start.elapsed();

    print_summary(&stats, elapsed);
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

// ====================
// テスト
// ====================

#[cfg(test)]
mod tests {
    use super::split_short_opt;

    /// h2load 互換: 値が密着した短オプション（`-c64`）を分解できること。
    ///
    /// これを弾いていたために `tools/perf/freebsd/run_perf_freebsd.sh`（`-t2 -c64` の形で
    /// 渡す）からの HTTP/3 計測が丸ごと 0 rps になっていた（2026-08-11 発覚）。
    #[test]
    fn split_short_opt_handles_attached_values() {
        assert_eq!(split_short_opt("-c64"), ("-c", Some("64")));
        assert_eq!(split_short_opt("-t2"), ("-t", Some("2")));
        assert_eq!(split_short_opt("-m32"), ("-m", Some("32")));
        assert_eq!(split_short_opt("-n128000"), ("-n", Some("128000")));
        assert_eq!(split_short_opt("-d10"), ("-d", Some("10")));
    }

    /// 分離形（`-c 64`）は従来どおり「フラグのみ」として扱われること。
    #[test]
    fn split_short_opt_keeps_separated_form() {
        for flag in ["-c", "-m", "-n", "-d", "-t"] {
            assert_eq!(split_short_opt(flag), (flag, None));
        }
    }

    /// 値を取らないオプション・長オプション・URL は分解しないこと。
    #[test]
    fn split_short_opt_leaves_other_args_intact() {
        assert_eq!(split_short_opt("-h"), ("-h", None));
        assert_eq!(split_short_opt("--help"), ("--help", None));
        assert_eq!(
            split_short_opt("https://127.0.0.1:4443/"),
            ("https://127.0.0.1:4443/", None)
        );
        // 値を取らない短オプションに文字が続く形は分解対象外（未知オプション扱いのまま）。
        assert_eq!(split_short_opt("-xyz"), ("-xyz", None));
    }
}
