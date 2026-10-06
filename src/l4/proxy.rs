//! L4 TCP プロキシ実装
//!
//! バイダイレクショナルストリーム転送、ロードバランシング、TLS パススルーを実装する。

use crate::config::{L4LbAlgorithm, L4ListenerConfig, L4TlsMode, CURRENT_CONFIG};
use crate::runtime::io::{AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt};
use crate::runtime::offload::offload;
// splice(2) は Linux 専用（設計ドキュメント 3.3 節）。BSD は forward_direction の
// ユーザースペース read/write 転送へフォールバックするため splice/Pipe は使わない。
#[cfg(target_os = "linux")]
use crate::runtime::splice::{splice, Pipe};
use crate::runtime::tcp::TcpStream as IoUringTcpStream;
use crate::runtime::time::timeout;

#[cfg(veil_ktls)]
use crate::ktls_rustls::{KtlsServerStream, RustlsAcceptor};
#[cfg(not(veil_ktls))]
use crate::simple_tls::{SimpleTlsAcceptor, SimpleTlsServerStream};
use ftlog::{debug, info, warn};
// std::io は splice 転送（Linux 専用経路）でのみ使用する。
#[allow(unused_imports)]
use crate::runtime::handle::AsRawFd;
#[cfg(target_os = "linux")]
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// L4 プロキシの接続数カウンタ（リスナーごと）
pub struct L4ConnectionCounter {
    pub current: AtomicUsize,
    pub total: AtomicUsize,
}

impl L4ConnectionCounter {
    pub fn new() -> Self {
        Self {
            current: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
        }
    }
}

impl Default for L4ConnectionCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// ラウンドロビン用アトミックカウンタ
pub struct RoundRobinState {
    counter: AtomicUsize,
}

impl RoundRobinState {
    pub fn new() -> Self {
        Self {
            counter: AtomicUsize::new(0),
        }
    }

    pub fn next(&self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        self.counter.fetch_add(1, Ordering::Relaxed) % len
    }

    pub fn next_weighted(&self, total_weight: usize) -> usize {
        if total_weight == 0 {
            return 0;
        }
        self.counter.fetch_add(1, Ordering::Relaxed) % total_weight
    }
}

impl Default for RoundRobinState {
    fn default() -> Self {
        Self::new()
    }
}

/// L4 上流アドレス（起動時解決済み、接続時に解決する未解決ホスト名、または UDS）。
///
/// UDS（`unix:<path>`、F-170）は TCP 上流のみ対象で UDP は対象外（設定検証で拒否する。
/// `src/config.rs` の L4 上流検証を参照）。
#[derive(Clone, Debug)]
pub enum L4UpstreamTarget {
    Resolved(SocketAddr),
    Unresolved(Arc<str>),
    /// UDS バックエンド（F-170）。ソケットパス（`unix:` 接頭辞は含まない）。
    #[cfg(unix)]
    Unix(Arc<str>),
}

/// L4 TCP 接続の接続先エンドポイント（F-170: `resolve_upstream_target` の唯一の戻り値型）。
#[derive(Clone, Debug)]
pub(crate) enum L4Endpoint {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(Arc<str>),
}

/// 設定ファイルのアドレス文字列を L4 上流ターゲットへ変換する。
///
/// 起動時に解決できるアドレスは `Resolved` としてキャッシュし、
/// DNS 未解決（B-33）のホスト名は `Unresolved` として保持して接続時に解決する。
/// `unix:<path>` 表記（F-170）は `Unix` として保持する（起動時解決不要）。
///
/// コールドパス（L4 リスナー起動時のみ）。同期 DNS は接続時 `resolve_upstream_target` の
/// `offload` へ退避し、ホットパスでは呼ばない。
#[allow(clippy::disallowed_methods)] // 起動時のみ: リスナー初期化で上流を Resolved/Unresolved に分類
pub fn parse_upstream_targets(config: &L4ListenerConfig) -> Vec<L4UpstreamTarget> {
    config
        .upstreams
        .iter()
        .map(|u| {
            #[cfg(unix)]
            if let Some(path) = u.addr.strip_prefix("unix:") {
                return L4UpstreamTarget::Unix(Arc::from(path));
            }
            if let Ok(addr) = u.addr.parse::<SocketAddr>() {
                return L4UpstreamTarget::Resolved(addr);
            }
            if let Ok(mut addrs) = u.addr.to_socket_addrs() {
                if let Some(addr) = addrs.next() {
                    return L4UpstreamTarget::Resolved(addr);
                }
            }
            L4UpstreamTarget::Unresolved(Arc::from(u.addr.as_str()))
        })
        .collect()
}

/// `runtime::offload` 専用ワーカー内でのみ呼ぶ（B-33 接続時 DNS 解決）。
#[allow(clippy::disallowed_methods)] // offload ワーカー内: ホットパスからは `resolve_upstream_target` 経由のみ
fn resolve_upstream_addr_sync(addr: &str) -> Option<SocketAddr> {
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Some(sa);
    }
    addr.to_socket_addrs().ok().and_then(|mut iter| iter.next())
}

/// 接続時に上流エンドポイントを解決する（未解決ホスト名は offload で DNS 解決、F-170: UDS 対応）。
///
/// `pub(crate)`: L4 UDP セッション確立（`crate::l4::udp`）からも利用する。UDP は
/// `L4Endpoint::Unix` を受け取っても接続できないため、呼び出し側（`l4/udp.rs`）は
/// `Unix` を「解決失敗」として扱う（設定検証で UDP 上流の `unix:` は既に拒否済みの
/// 想定だが、念のための防御）。
pub(crate) async fn resolve_upstream_target(target: &L4UpstreamTarget) -> Option<L4Endpoint> {
    match target {
        L4UpstreamTarget::Resolved(addr) => Some(L4Endpoint::Tcp(*addr)),
        L4UpstreamTarget::Unresolved(addr) => {
            let addr = Arc::clone(addr);
            offload(move || resolve_upstream_addr_sync(&addr))
                .await
                .map(L4Endpoint::Tcp)
        }
        #[cfg(unix)]
        L4UpstreamTarget::Unix(path) => Some(L4Endpoint::Unix(Arc::clone(path))),
    }
}

/// `L4Endpoint` へ接続する唯一の入口（F-170: TCP/UDS 両対応）。
/// TCP 経路（`L4Endpoint::Tcp`）の命令列は F-170 以前と不変。
async fn connect_l4_endpoint(endpoint: &L4Endpoint) -> std::io::Result<IoUringTcpStream> {
    match endpoint {
        L4Endpoint::Tcp(addr) => IoUringTcpStream::connect(*addr).await,
        #[cfg(unix)]
        L4Endpoint::Unix(path) => {
            IoUringTcpStream::connect_unix(std::path::Path::new(path.as_ref())).await
        }
    }
}

/// 設定ファイルのアドレス文字列を起動時に `SocketAddr` へ変換して保持する。
///
/// 全上流が起動時に解決可能な場合のみ成功（単体テスト・後方互換用）。UDS 上流は
/// `SocketAddr` で表現できないため常にエラーとする。
pub fn parse_upstream_addrs(config: &L4ListenerConfig) -> Result<Vec<SocketAddr>, String> {
    parse_upstream_targets(config)
        .into_iter()
        .map(|target| match target {
            L4UpstreamTarget::Resolved(addr) => Ok(addr),
            L4UpstreamTarget::Unresolved(addr) => {
                resolve_upstream_addr_sync(&addr).ok_or_else(|| {
                    format!(
                        "failed to parse upstream addr '{}': name resolution failed",
                        addr
                    )
                })
            }
            #[cfg(unix)]
            L4UpstreamTarget::Unix(path) => Err(format!(
                "upstream 'unix:{}' cannot be represented as a SocketAddr",
                path
            )),
        })
        .collect()
}

/// upstream アドレスを選択する（ロードバランシング）
///
/// 戻り値: `(upstream_index, &str)` — インデックスは `parsed_addrs` への参照用、
/// `&str` はログ用（`config` からの借用のため追加アロケーション不要）。
pub fn select_upstream<'a>(
    config: &'a L4ListenerConfig,
    rr_state: &RoundRobinState,
    conn_counters: &[AtomicUsize],
    health_state: &[AtomicBool],
) -> Option<(usize, &'a str)> {
    if config.upstreams.is_empty() {
        return None;
    }

    match config.lb {
        L4LbAlgorithm::RoundRobin => {
            // Weighted Round Robin: unhealthy upstream を weight 計算から除外
            let total_weight: usize = config
                .upstreams
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    health_state
                        .get(*i)
                        .is_none_or(|h| h.load(Ordering::Relaxed))
                })
                .map(|(_, u)| u.weight.max(1) as usize)
                .sum();

            if total_weight == 0 {
                return None;
            }

            let slot = rr_state.next_weighted(total_weight);
            let mut cumulative = 0usize;
            for (i, upstream) in config.upstreams.iter().enumerate() {
                let is_healthy = health_state
                    .get(i)
                    .is_none_or(|h| h.load(Ordering::Relaxed));
                if !is_healthy {
                    continue;
                }
                cumulative += upstream.weight.max(1) as usize;
                if slot < cumulative {
                    return Some((i, upstream.addr.as_str()));
                }
            }
            // フォールバック: 最後の healthy upstream
            for (i, upstream) in config.upstreams.iter().enumerate().rev() {
                if health_state
                    .get(i)
                    .is_none_or(|h| h.load(Ordering::Relaxed))
                {
                    return Some((i, upstream.addr.as_str()));
                }
            }
            None
        }
        L4LbAlgorithm::LeastConn => config
            .upstreams
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                health_state
                    .get(*i)
                    .is_none_or(|h| h.load(Ordering::Relaxed))
            })
            .min_by_key(|(i, _)| {
                conn_counters
                    .get(*i)
                    .map_or(0, |c| c.load(Ordering::Relaxed))
            })
            .map(|(i, u)| (i, u.addr.as_str())),
    }
}

/// バッファサイズ（64KB: io_uring の一般的な推奨値）
const BUF_SIZE: usize = 64 * 1024;

/// c→u / u→c の 2 方向転送を同時に走らせ、両方の転送バイト数を返す。
///
/// `futures::join!` は**どちらか片方が起床するたびに両方を poll する**（子ごとの Waker を
/// 持たない）。reactor（kqueue/epoll）の `read`/`readable` は poll されるたびに
/// まず `read(2)`/`poll(2)` を試すので、起床していない側が毎回 EAGAIN を空打ちしていた
/// （FreeBSD aarch64 の `l4_tcp` 3B で `read` 6.0/req = 1 方向 3 回のうち 1 回が空打ち）。
/// `FuturesUnordered` は起床した子だけを poll する。子タスクのノード確保は接続あたり 2 回で、
/// リクエストごとには発生しない（既存の 64KB 転送バッファと同じ接続単位の固定費）。
async fn join_directions<F>(c2u: F, u2c: F) -> (usize, usize)
where
    F: std::future::Future<Output = usize>,
{
    use futures::stream::{FuturesUnordered, StreamExt};
    use futures::FutureExt;
    // 2 つの `map` が同じ型になるよう、クロージャではなく関数ポインタで方向を付ける。
    fn tag_c2u(n: usize) -> (bool, usize) {
        (true, n)
    }
    fn tag_u2c(n: usize) -> (bool, usize) {
        (false, n)
    }
    let mut set = FuturesUnordered::new();
    set.push(c2u.map(tag_c2u as fn(usize) -> (bool, usize)));
    set.push(u2c.map(tag_u2c as fn(usize) -> (bool, usize)));
    let (mut c2u_bytes, mut u2c_bytes) = (0, 0);
    while let Some((is_c2u, n)) = set.next().await {
        if is_c2u {
            c2u_bytes = n;
        } else {
            u2c_bytes = n;
        }
    }
    (c2u_bytes, u2c_bytes)
}

/// 1方向の転送ループ
///
/// バッファはこの関数内でコネクション確立時に **一度だけ** 確保し、
/// read/write の ownership handoff を利用してループ内での再確保を完全に排除する。
///
/// ショートライト時は `copy_within` でバッファ内シフトを行い、追加アロケーション不要。
async fn forward_direction(
    src: &IoUringTcpStream,
    dst: &IoUringTcpStream,
    idle_timeout: Duration,
    name: &str,
) -> usize {
    // コネクションあたり一度だけ確保。read()/write() が ownership を受け取って返すため
    // ループ内で再確保されることはない。
    // clippy::uninit_vec 許容理由: カーネルが read(2) で先頭 n バイトを上書きし、直後の
    // set_len(n) 切り詰めにより未初期化領域を Rust 側から一切読まない。ゼロ初期化は
    // コネクションごとに 64KB の memset を追加するためホットパス絶対規則により回避する。
    #[allow(clippy::uninit_vec)]
    let mut buf: Vec<u8> = {
        let mut b = Vec::with_capacity(BUF_SIZE);
        // SAFETY: capacity ぶんの領域は確保済み。カーネルが read で上書きするため
        // Rust 側から [len..capacity] を読まない限り安全。
        unsafe { b.set_len(BUF_SIZE) };
        b
    };

    let mut total = 0usize;
    'outer: loop {
        // read(): buf の ownership を渡し、完了後に (result, buf) として返ってくる
        let (res, mut b) = match timeout(idle_timeout, src.read(buf)).await {
            Ok(r) => r,
            Err(_) => {
                debug!("[L4:{}] idle timeout", name);
                break;
            }
        };
        let n = match res {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        // ReadFuture<Vec<u8>> の set_init は grow-only のため、read 後も b.len() は BUF_SIZE の
        // ままで n には縮まらない。実際に読み取った n バイトのみを送信するよう長さを切り詰める。
        // これを怠ると未初期化の末尾バイトまで送信して転送データを破損させる（生 TCP 中継では
        // 致命的: TLS パススルー時にバックエンドがハンドシェイクを拒否して u→c が 0 になる）。
        unsafe { b.set_len(n) };

        // ショートライトをリトライ（追加アロケーションなし）
        let mut written = 0;
        while written < n {
            if written > 0 {
                // 未送信バイトをバッファ先頭にシフト（memmove 相当、ゼロコピー）
                b.copy_within(written..n, 0);
                unsafe { b.set_len(n - written) };
            }
            let (wres, wb) = dst.write(b).await;
            b = wb;
            match wres {
                Ok(0) | Err(_) => break 'outer,
                Ok(wn) => written += wn,
            }
        }
        total += n;

        // 次の read のためにバッファ長を BUF_SIZE に戻す
        // (capacity は変わらないため再確保は発生しない)
        unsafe { b.set_len(BUF_SIZE) };
        buf = b;
    }

    // B-45: 転送ループを抜けるすべての経路（src EOF・read/write エラー・アイドル
    // タイムアウト）で dst へ半クローズ（FIN）を伝搬する。これにより対向方向の
    // 転送ループもアイドルタイムアウトを待たずに即座に EOF で終了し、セッションの
    // fd（ソケット 2 本）が速やかに解放される（TCP 半クローズ意味論は保持: dst の
    // 送信方向を止めるだけで、dst からの受信＝逆方向の src には影響しない）。
    // shutdown(2) はノンブロッキングな syscall で io_uring の新規オペコードを要さず、
    // 戻り値は無視してよい（対向がすでにクローズ済みの ENOTCONN 等は無害）。
    let _ = dst.shutdown(std::net::Shutdown::Write);
    total
}

/// WASM network filter 適用ありの 1 方向転送ループ（F-133）。
///
/// `bidirectional_forward` から WASM モジュールが設定されている場合のみ呼ばれる
/// （呼び出し側で 1 回だけ判定済み）。splice ゼロコピー経路は使わず、読み取ったバイト列を
/// 都度 `FilterEngine` へ渡してから書き込む。WASM に生データを見せる必要があるための
/// 意図的なユーザー空間バッファ経由経路であり、ホットパス絶対規則の「WASM 無効時は
/// コスト増なし」はこの関数を呼ばないことで満たす（無効時は `forward_direction`/
/// `forward_direction_splice` のみが動く）。
#[cfg(feature = "wasm")]
async fn forward_direction_wasm(
    src: &IoUringTcpStream,
    dst: &IoUringTcpStream,
    idle_timeout: Duration,
    name: &str,
    wasm_modules: &[crate::wasm_plugin_config::ModuleRef],
    is_downstream_data: bool,
    close_requested: &Arc<AtomicBool>,
) -> usize {
    #[allow(clippy::uninit_vec)]
    let mut buf: Vec<u8> = {
        let mut b = Vec::with_capacity(BUF_SIZE);
        // SAFETY: read(2) が先頭 n バイトを上書きし、直後の set_len(n) で切り詰めるため
        // 未初期化領域を Rust 側から読まない。
        unsafe { b.set_len(BUF_SIZE) };
        b
    };

    let mut total = 0usize;
    'outer: loop {
        // B-57: `futures::join!` は c→u/u→c の 2 方向を同一タスク内で協調動作させるだけで、
        // 一方が WASM フィルタから `NetworkAction::Close` を受け取っても他方には伝わらない
        // （Proxy-Wasm の `proxy_close_stream` は「コネクション全体」を終了させる意味論の
        // はずが、片方向だけ止まり、もう片方はバックエンド応答をクライアントへ転送し続けて
        // しまうバグがあった）。`close_requested` は接続ごとに 1 個確保する `Arc<AtomicBool>`
        // で、ループ 1 周につき 1 回のアトミック load のみを追加する（ロック・アロケーション
        // 増加なし）。対向方向が既に close 済みならここで即座に自分も転送を止める。
        if close_requested.load(Ordering::SeqCst) {
            debug!(
                "[L4:{}] peer direction requested close, stopping forward (wasm)",
                name
            );
            break;
        }

        let (res, mut b) = match timeout(idle_timeout, src.read(buf)).await {
            Ok(r) => r,
            Err(_) => {
                debug!("[L4:{}] idle timeout (wasm)", name);
                break;
            }
        };
        let n = match res {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        unsafe { b.set_len(n) };

        // WASM フィルタ経由（この経路自体が「ユーザー空間バッファを通す」選択の結果
        // なので、Bytes 化・書き換え時のコピーは許容する）。読み取りバッファ `b` は
        // 次の read で使い回すため copy_from_slice でコピーし、b の所有権は手元に残す。
        let data = bytes::Bytes::copy_from_slice(&b);
        let engine = CURRENT_CONFIG.load().wasm_filter_engine.clone();
        let out = if let Some(engine) = engine {
            let result = if is_downstream_data {
                engine
                    .on_downstream_data_with_modules(wasm_modules, data.clone(), false)
                    .await
            } else {
                engine
                    .on_upstream_data_with_modules(wasm_modules, data.clone(), false)
                    .await
            };
            match result {
                crate::wasm::NetworkFilterResult::Continue { data } => data,
                crate::wasm::NetworkFilterResult::Close => {
                    debug!("[L4:{}] WASM module requested connection close", name);
                    // B-57: Close はコネクション全体の終了を意味する。自分のループを
                    // 抜けるだけでなく (1) 対向方向へ知らせるフラグを立て、(2) 対向方向が
                    // ブロックしている可能性のある read を即座に起こすため src/dst 双方を
                    // shutdown(Both) する。対向方向の src は自分の dst と、対向方向の dst は
                    // 自分の src と同一ソケットなので、この 2 回の shutdown だけで両ソケット
                    // とも読み書き不可になり、対向方向の read はブロックせず即座に
                    // 0 バイト/エラーで返る（shutdown(2) は新規 io_uring オペコードを要さない
                    // 非ブロッキング syscall）。
                    close_requested.store(true, Ordering::SeqCst);
                    let _ = src.shutdown(std::net::Shutdown::Both);
                    let _ = dst.shutdown(std::net::Shutdown::Both);
                    break 'outer;
                }
                crate::wasm::NetworkFilterResult::Pause => {
                    warn!(
                        "[L4:{}] WASM module requested Pause on network data, \
                         but async resume is not yet supported for L4; continuing",
                        name
                    );
                    data
                }
            }
        } else {
            data
        };

        // B-57: フィルタ呼び出しは非同期（ホスト関数呼び出し中に await で他方向へ
        // 制御が渡り得る）ため、自分がここに到達するまでの間に対向方向が close を
        // 要求している可能性がある。書き込み前に再チェックし、要求されていれば
        // このデータは転送せずに終了する（対向方向 close 後にバックエンド応答が
        // クライアントへ届いてしまう F-133 由来のバグ、B-57、を防ぐ）。
        if close_requested.load(Ordering::SeqCst) {
            debug!(
                "[L4:{}] peer direction requested close, dropping already-read data (wasm)",
                name
            );
            break;
        }

        let out_len = out.len();
        let mut pending = out.to_vec();
        let mut written = 0usize;
        while written < out_len {
            if written > 0 {
                pending.copy_within(written..out_len, 0);
                pending.truncate(out_len - written);
            }
            let (wres, wb) = dst.write(pending).await;
            pending = wb;
            match wres {
                Ok(0) | Err(_) => break 'outer,
                Ok(wn) => written += wn,
            }
        }
        total += n;

        // 次の read のためにバッファ長を BUF_SIZE に戻す（capacity は変わらないため
        // 再確保は発生しない。b は read() から返ってきた元の Vec の ownership）。
        unsafe { b.set_len(BUF_SIZE) };
        buf = b;
    }

    let _ = dst.shutdown(std::net::Shutdown::Write);
    total
}

// ====================
// L4 splice パイプのスレッドローカルプール（F-40）
// ====================
//
// 接続ごとに `pipe2(2)` を発行するコスト（システムコール + fd 確保）をホットパスから
// 排除するため、使い終わったパイプをスレッドローカルに保持して再利用する。
// パイプに残データがあるまま再利用すると**次の接続へデータが混線する**ため、
// 返却時に FIONREAD で空であることを確認できた場合のみプールへ戻す（それ以外は破棄）。

#[cfg(target_os = "linux")]
thread_local! {
    static L4_PIPE_POOL: std::cell::RefCell<Vec<Pipe>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// プールに保持するパイプ本数の上限（スレッドごと）。
/// 1 接続あたり 2 本（上り/下り）使うため、同時 32 接続分をカバーする。
#[cfg(target_os = "linux")]
const L4_PIPE_POOL_MAX: usize = 64;

/// プールからパイプを取得する。空ならば新規作成（`pipe2(2)`）にフォールバック。
#[cfg(target_os = "linux")]
fn acquire_pipe() -> io::Result<Pipe> {
    if let Some(p) = L4_PIPE_POOL.with(|pool| pool.borrow_mut().pop()) {
        return Ok(p);
    }
    Pipe::new()
}

/// パイプをプールへ返却する。
///
/// 残データがあるパイプを再利用すると次接続へデータが漏れる（混線する）ため、
/// FIONREAD で空を確認できた場合のみ返却し、それ以外（残データあり・ioctl 失敗・
/// プール満杯）は Drop に任せて破棄する。
#[cfg(target_os = "linux")]
fn release_pipe(pipe: Pipe) {
    let mut pending: libc::c_int = 0;
    let ret = unsafe { libc::ioctl(pipe.read_fd, libc::FIONREAD, &mut pending) };
    if ret != 0 || pending != 0 {
        // 破棄（Drop が fd をクローズ）
        return;
    }
    L4_PIPE_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.len() < L4_PIPE_POOL_MAX {
            pool.push(pipe);
        }
        // 満杯なら Drop で破棄
    });
}

/// 1 方向の splice ゼロコピー転送ループ（F-30）。
///
/// `src(socket) → pipe → dst(socket)` の 2 段 splice でカーネル内転送する。ユーザースペースの
/// バッファを一切経由しない（メモリコピー・ヒープ確保なし）。`readable()`/`writable()`
/// （POLL_ADD）で待機し、ノンブロッキング splice をドレインループで回す（エッジトリガ）。
#[cfg(target_os = "linux")]
async fn forward_direction_splice(
    src: &IoUringTcpStream,
    dst: &IoUringTcpStream,
    pipe: &Pipe,
    idle_timeout: Duration,
    name: &str,
) -> usize {
    let src_fd = src.as_raw_fd();
    let dst_fd = dst.as_raw_fd();
    let mut total = 0usize;

    'outer: loop {
        // src が読み取り可能になるまで待つ（アイドルタイムアウト付き）。
        match timeout(idle_timeout, src.readable()).await {
            Ok(Ok(())) => {}
            _ => {
                debug!("[L4:{}] idle timeout", name);
                break;
            }
        }

        // 読めるだけ src → pipe → dst へ流す（EAGAIN になるまでドレイン）。
        loop {
            let n = match splice(src_fd, pipe.write_fd, BUF_SIZE).await {
                Ok(0) => break 'outer, // src が EOF（接続クローズ）
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break, // 出尽くした → 再 poll
                Err(_) => break 'outer,
            };

            // pipe にある n バイトを dst へすべて流す（dst のバックプレッシャに対応）。
            let mut moved = 0;
            while moved < n {
                match splice(pipe.read_fd, dst_fd, n - moved).await {
                    Ok(0) => break 'outer, // dst クローズ
                    Ok(m) => moved += m,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // dst 送信バッファ満杯 → 書き込み可能になるまで待つ。
                        if timeout(idle_timeout, dst.writable()).await.is_err() {
                            break 'outer;
                        }
                    }
                    Err(_) => break 'outer,
                }
            }
            total += n;
        }
    }

    // B-45: 転送ループを抜けるすべての経路（src EOF・splice エラー・アイドル
    // タイムアウト）で dst へ半クローズ（FIN）を伝搬する。これにより対向方向の
    // 転送ループもアイドルタイムアウトを待たずに即座に EOF で終了し、セッションの
    // fd（ソケット 2 本 + splice パイプ 2 本）が速やかに解放される（TCP 半クローズ
    // 意味論は保持: dst の送信方向を止めるだけで、dst からの受信＝逆方向の src には
    // 影響しない）。shutdown(2) はノンブロッキングな syscall で io_uring の新規
    // オペコードを要さず、戻り値は無視してよい（対向がすでにクローズ済みの
    // ENOTCONN 等は無害）。
    let _ = unsafe { libc::shutdown(dst_fd, libc::SHUT_WR) };
    total
}

/// クライアントと upstream の間でバイダイレクショナル転送を行う。
///
/// 各方向に pipe を 1 本割り当て、`splice(2)`（`IORING_OP_SPLICE`）でカーネル内ゼロコピー
/// 転送する。ユーザースペースのバッファ確保・コピーが発生しない。pipe 作成に失敗した場合
/// （fd 上限等）のみ、従来のユーザースペース read/write フォールバックを使う。
pub async fn bidirectional_forward(
    client: IoUringTcpStream,
    upstream: IoUringTcpStream,
    idle_timeout: Duration,
    listener_name: &str,
    #[cfg(feature = "wasm")] wasm_modules: &[crate::wasm_plugin_config::ModuleRef],
) {
    // F-133: WASM network filter が設定されているかどうかを接続確立時に 1 回だけ判定する
    // （ホットパス絶対規則: WASM 無効時はこの分岐 1 つだけがコストのすべてで、以降は
    // 従来どおり splice/forward_direction のゼロコピー経路をそのまま使う）。
    #[cfg(feature = "wasm")]
    if !wasm_modules.is_empty() {
        // WASM 有効時は splice を使わず、データを可視化できるユーザー空間バッファ経由の
        // 転送へ切り替える（design doc F-133: 「splice/zero-copy 経路は WASM 無効時のみ」）。
        let engine = CURRENT_CONFIG.load().wasm_filter_engine.clone();
        if let Some(ref engine) = engine {
            match engine.on_new_connection_with_modules(wasm_modules).await {
                crate::wasm::NetworkAction::Close => {
                    debug!(
                        "[L4:{}] WASM module closed connection on_new_connection",
                        listener_name
                    );
                    return;
                }
                crate::wasm::NetworkAction::Continue => {}
            }
        }

        // B-57: 2 方向間で close 要求を共有するためのフラグ。接続ごとに 1 回だけ確保する
        // （ホットパスへの割り込みはこの `Arc::new` 1 回のみで、ループ内では発生しない）。
        let close_requested = Arc::new(AtomicBool::new(false));
        let (c2u_bytes, u2c_bytes) = futures::join!(
            forward_direction_wasm(
                &client,
                &upstream,
                idle_timeout,
                listener_name,
                wasm_modules,
                true,
                &close_requested,
            ),
            forward_direction_wasm(
                &upstream,
                &client,
                idle_timeout,
                listener_name,
                wasm_modules,
                false,
                &close_requested,
            )
        );
        debug!(
            "[L4:{}] connection closed (wasm): c→u {} bytes, u→c {} bytes",
            listener_name, c2u_bytes, u2c_bytes
        );
        if let Some(ref engine) = engine {
            engine.on_downstream_close_with_modules(wasm_modules).await;
            engine.on_upstream_close_with_modules(wasm_modules).await;
        }
        return;
    }

    // BSD（`splice(2)` 非搭載）: 設計ドキュメント 3.3 節のとおり、splice/パイプ経路は
    // Linux 専用のため使わず、常にユーザースペース read/write 転送（`forward_direction`。
    // コネクション確立時に一度だけバッファを確保しリクエストごとの再確保は発生しない）
    // へフォールバックする。
    #[cfg(not(target_os = "linux"))]
    {
        let (c2u_bytes, u2c_bytes) = join_directions(
            forward_direction(&client, &upstream, idle_timeout, listener_name),
            forward_direction(&upstream, &client, idle_timeout, listener_name),
        )
        .await;
        debug!(
            "[L4:{}] connection closed: c→u {} bytes, u→c {} bytes",
            listener_name, c2u_bytes, u2c_bytes
        );
        return;
    }

    // join_directions は両 Future を同一タスク内でインターリーブするため、
    // &TcpStream / &Pipe の同時借用は安全。
    // パイプはスレッドローカルプール（F-40）から取得し、接続ごとの pipe2(2) を排除する。
    #[cfg(target_os = "linux")]
    match (acquire_pipe(), acquire_pipe()) {
        (Ok(c2u_pipe), Ok(u2c_pipe)) => {
            let (c2u_bytes, u2c_bytes) = join_directions(
                forward_direction_splice(
                    &client,
                    &upstream,
                    &c2u_pipe,
                    idle_timeout,
                    listener_name,
                ),
                forward_direction_splice(
                    &upstream,
                    &client,
                    &u2c_pipe,
                    idle_timeout,
                    listener_name,
                ),
            )
            .await;
            debug!(
                "[L4:{}] connection closed (splice): c→u {} bytes, u→c {} bytes",
                listener_name, c2u_bytes, u2c_bytes
            );
            // 空であることを確認できたパイプのみ再利用のためプールへ返却する。
            release_pipe(c2u_pipe);
            release_pipe(u2c_pipe);
        }
        _ => {
            // pipe 作成失敗時はユーザースペースコピーへフォールバック。
            warn!(
                "[L4:{}] pipe creation failed; falling back to userspace copy",
                listener_name
            );
            let (c2u_bytes, u2c_bytes) = join_directions(
                forward_direction(&client, &upstream, idle_timeout, listener_name),
                forward_direction(&upstream, &client, idle_timeout, listener_name),
            )
            .await;
            debug!(
                "[L4:{}] connection closed: c→u {} bytes, u→c {} bytes",
                listener_name, c2u_bytes, u2c_bytes
            );
        }
    }
}

/// L4 接続を処理する（upstream 選択 → 接続 → バイダイレクショナル転送）
pub async fn handle_l4_connection(
    client: IoUringTcpStream,
    peer_addr: SocketAddr,
    config: Arc<L4ListenerConfig>,
    upstream_targets: Arc<Vec<L4UpstreamTarget>>,
    rr_state: Arc<RoundRobinState>,
    conn_counters: Arc<Vec<AtomicUsize>>,
    listener_counter: Arc<L4ConnectionCounter>,
    health_state: Arc<Vec<AtomicBool>>,
) {
    // 接続数制限チェック
    if config.max_connections > 0 {
        let current = listener_counter.current.load(Ordering::Relaxed);
        if current >= config.max_connections {
            warn!(
                "[L4:{}] connection limit ({}) reached, rejecting {}",
                config.name, config.max_connections, peer_addr
            );
            return;
        }
    }

    listener_counter.current.fetch_add(1, Ordering::Relaxed);
    listener_counter.total.fetch_add(1, Ordering::Relaxed);

    struct ListenerGuard(Arc<L4ConnectionCounter>);
    impl Drop for ListenerGuard {
        fn drop(&mut self) {
            self.0.current.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _listener_guard = ListenerGuard(listener_counter.clone());

    if config.tls == L4TlsMode::Terminate {
        if l4_server_tls_config().is_none() {
            warn!(
                "[L4:{}] TLS terminate requires server TLS configuration",
                config.name
            );
            return;
        }
        handle_l4_tls_terminate_connection(
            client,
            peer_addr,
            config,
            upstream_targets,
            rr_state,
            conn_counters,
            health_state,
        )
        .await;
        return;
    }

    // upstream 選択（&str は config からの借用、追加アロケーションなし）
    let (upstream_idx, upstream_addr_str) =
        match select_upstream(&config, &rr_state, &conn_counters, &health_state) {
            Some(pair) => pair,
            None => {
                warn!("[L4:{}] no healthy upstream available", config.name);
                return;
            }
        };

    // 選択した upstream の接続数をインクリメント
    if let Some(c) = conn_counters.get(upstream_idx) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    struct UpstreamGuard {
        counters: Arc<Vec<AtomicUsize>>,
        idx: usize,
    }
    impl Drop for UpstreamGuard {
        fn drop(&mut self) {
            if let Some(c) = self.counters.get(self.idx) {
                c.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
    let _upstream_guard = UpstreamGuard {
        counters: conn_counters.clone(),
        idx: upstream_idx,
    };

    let endpoint = match upstream_targets.get(upstream_idx) {
        Some(target) => match resolve_upstream_target(target).await {
            Some(a) => a,
            None => {
                warn!(
                    "[L4:{}] failed to resolve upstream {}",
                    config.name, upstream_addr_str
                );
                return;
            }
        },
        None => {
            warn!(
                "[L4:{}] upstream_targets index {} out of range",
                config.name, upstream_idx
            );
            return;
        }
    };

    let connect_timeout = Duration::from_secs(config.connect_timeout_secs);
    let upstream = match timeout(connect_timeout, connect_l4_endpoint(&endpoint)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            warn!(
                "[L4:{}] failed to connect to upstream {}: {}",
                config.name, upstream_addr_str, e
            );
            return;
        }
        Err(_) => {
            warn!(
                "[L4:{}] connection to upstream {} timed out",
                config.name, upstream_addr_str
            );
            return;
        }
    };

    let _ = upstream.set_nodelay(true);

    info!(
        "[L4:{}] {} → {} (tls={:?})",
        config.name, peer_addr, upstream_addr_str, config.tls
    );

    let idle_timeout = Duration::from_secs(config.idle_timeout_secs);
    bidirectional_forward(
        client,
        upstream,
        idle_timeout,
        &config.name,
        #[cfg(feature = "wasm")]
        &config.resolved_wasm_modules,
    )
    .await;
}

/// L4 TLS 終端用のサーバー TLS 設定を取得する。
fn l4_server_tls_config() -> Option<std::sync::Arc<rustls::ServerConfig>> {
    crate::tls_reload::current_global_tls_config().or_else(|| {
        CURRENT_CONFIG
            .load()
            .tls_config
            .as_ref()
            .map(std::sync::Arc::clone)
    })
}

/// クライアント側 TLS ハンドシェイクを完了し、平文ストリームへ復号する。
#[cfg(veil_ktls)]
async fn accept_l4_tls_client(
    client: IoUringTcpStream,
    handshake_timeout: Duration,
    listener_name: &str,
    peer_addr: SocketAddr,
) -> Option<KtlsServerStream> {
    let tls_config = l4_server_tls_config()?;
    let runtime = CURRENT_CONFIG.load();
    let acceptor = RustlsAcceptor::new(tls_config)
        .with_ktls(runtime.ktls_config.enabled)
        .with_fallback(runtime.ktls_config.fallback_enabled)
        .with_tcp_cork(runtime.ktls_config.tcp_cork_enabled);

    match timeout(handshake_timeout, acceptor.accept(client, None)).await {
        Ok(Ok(stream)) => Some(stream),
        Ok(Err(e)) => {
            warn!(
                "[L4:{}] TLS handshake failed from {}: {}",
                listener_name, peer_addr, e
            );
            None
        }
        Err(_) => {
            warn!(
                "[L4:{}] TLS handshake timed out from {}",
                listener_name, peer_addr
            );
            None
        }
    }
}

#[cfg(not(veil_ktls))]
async fn accept_l4_tls_client(
    client: IoUringTcpStream,
    handshake_timeout: Duration,
    listener_name: &str,
    peer_addr: SocketAddr,
) -> Option<SimpleTlsServerStream> {
    let tls_config = l4_server_tls_config()?;
    let acceptor = SimpleTlsAcceptor::new(tls_config);

    match timeout(handshake_timeout, acceptor.accept(client, None)).await {
        Ok(Ok(stream)) => Some(stream),
        Ok(Err(e)) => {
            warn!(
                "[L4:{}] TLS handshake failed from {}: {}",
                listener_name, peer_addr, e
            );
            None
        }
        Err(_) => {
            warn!(
                "[L4:{}] TLS handshake timed out from {}",
                listener_name, peer_addr
            );
            None
        }
    }
}

/// TLS 終端後のクライアント平文ストリームと upstream 間の双方向転送。
///
/// splice は平文 fd が両端に揃う場合のみ使えるため、TLS 復号後は WebSocket 転送と同様の
/// ポーリングループで実装する（`&mut` クライアントと `&` upstream の同時借用を避ける）。
async fn bidirectional_forward_tls_terminate<C>(
    client: &mut C,
    upstream: &IoUringTcpStream,
    idle_timeout: Duration,
    listener_name: &str,
    #[cfg(feature = "wasm")] wasm_modules: &[crate::wasm_plugin_config::ModuleRef],
) where
    C: AsyncReadRent + AsyncWriteRent + Unpin,
{
    // F-133: WASM network filter が設定されているかどうかを接続確立時に 1 回だけ判定する
    // （ホットパス絶対規則: 未設定時はこの分岐 1 つだけがコストのすべてで、以降は
    // 従来どおりのユーザー空間ポーリングループをそのまま使う。TLS 終端経路はもともと
    // splice を使わない平文コピーのため、WASM 有効時も追加のバッファ経路切替は不要で
    // 同じループへ WASM 呼び出しを差し込むだけで済む）。
    #[cfg(feature = "wasm")]
    let wasm_active = !wasm_modules.is_empty();

    #[cfg(feature = "wasm")]
    let engine = if wasm_active {
        CURRENT_CONFIG.load().wasm_filter_engine.clone()
    } else {
        None
    };

    #[cfg(feature = "wasm")]
    if let Some(ref engine) = engine {
        match engine.on_new_connection_with_modules(wasm_modules).await {
            crate::wasm::NetworkAction::Close => {
                debug!(
                    "[L4:{}] WASM module closed connection on_new_connection (tls terminate)",
                    listener_name
                );
                return;
            }
            crate::wasm::NetworkAction::Continue => {}
        }
    }

    #[allow(clippy::uninit_vec)]
    let make_buf = || {
        let mut b = Vec::with_capacity(BUF_SIZE);
        // SAFETY: read で上書きするため未初期化領域は読まない。
        unsafe { b.set_len(BUF_SIZE) };
        b
    };

    'outer: loop {
        let mut had_activity = false;

        let buf = make_buf();
        match timeout(idle_timeout, client.read(buf)).await {
            Ok((Ok(0), _)) => break,
            Ok((Ok(n), mut b)) => {
                unsafe { b.set_len(n) };

                // downstream (client → upstream) データを WASM フィルタへ通す
                // （`wasm_active` が false の場合はこのブロック自体を跳ばし、
                // 従来どおり b をそのまま転送する）。
                #[cfg(feature = "wasm")]
                if wasm_active {
                    let data = bytes::Bytes::copy_from_slice(&b[..n]);
                    if let Some(ref engine) = engine {
                        let result = engine
                            .on_downstream_data_with_modules(wasm_modules, data, false)
                            .await;
                        match result {
                            crate::wasm::NetworkFilterResult::Continue { data } => {
                                // WASM が書き換えた（あるいは未変更の）データで b の内容を
                                // 置き換える。capacity は BUF_SIZE だが、モジュールが長さを
                                // 変えた場合でも安全なように clear + extend_from_slice で扱う
                                // （超過時のみ再確保、通常は同容量内で完結）。
                                b.clear();
                                b.extend_from_slice(&data);
                            }
                            crate::wasm::NetworkFilterResult::Close => {
                                debug!(
                                    "[L4:{}] WASM module requested connection close (tls terminate, downstream)",
                                    listener_name
                                );
                                break 'outer;
                            }
                            crate::wasm::NetworkFilterResult::Pause => {
                                warn!(
                                    "[L4:{}] WASM module requested Pause on network data, \
                                     but async resume is not yet supported for L4; continuing",
                                    listener_name
                                );
                            }
                        }
                    }
                }

                let n = b.len();
                let mut pending = b;
                let mut written = 0usize;
                while written < n {
                    if written > 0 {
                        pending.copy_within(written..n, 0);
                        unsafe { pending.set_len(n - written) };
                    }
                    let (wres, returned) = upstream.write(pending).await;
                    pending = returned;
                    match wres {
                        Ok(0) | Err(_) => break 'outer,
                        Ok(wn) => written += wn,
                    }
                }
                had_activity = true;
            }
            Ok((Err(_), _)) => break,
            Err(_) => {}
        }

        let buf = make_buf();
        match timeout(idle_timeout, upstream.read(buf)).await {
            Ok((Ok(0), _)) => break,
            Ok((Ok(n), mut b)) => {
                unsafe { b.set_len(n) };

                // upstream (backend → client) データを WASM フィルタへ通す。
                #[cfg(feature = "wasm")]
                if wasm_active {
                    let data = bytes::Bytes::copy_from_slice(&b[..n]);
                    if let Some(ref engine) = engine {
                        let result = engine
                            .on_upstream_data_with_modules(wasm_modules, data, false)
                            .await;
                        match result {
                            crate::wasm::NetworkFilterResult::Continue { data } => {
                                // WASM が書き換えた（あるいは未変更の）データで b の内容を
                                // 置き換える。capacity は BUF_SIZE だが、モジュールが長さを
                                // 変えた場合でも安全なように clear + extend_from_slice で扱う
                                // （超過時のみ再確保、通常は同容量内で完結）。
                                b.clear();
                                b.extend_from_slice(&data);
                            }
                            crate::wasm::NetworkFilterResult::Close => {
                                debug!(
                                    "[L4:{}] WASM module requested connection close (tls terminate, upstream)",
                                    listener_name
                                );
                                break 'outer;
                            }
                            crate::wasm::NetworkFilterResult::Pause => {
                                warn!(
                                    "[L4:{}] WASM module requested Pause on network data, \
                                     but async resume is not yet supported for L4; continuing",
                                    listener_name
                                );
                            }
                        }
                    }
                }

                let (wres, _) = client.write_all(b).await;
                if wres.is_err() {
                    break;
                }
                had_activity = true;
            }
            Ok((Err(_), _)) => break,
            Err(_) => {}
        }

        if !had_activity {
            debug!("[L4:{}] TLS terminate idle timeout", listener_name);
            break;
        }
    }

    #[cfg(feature = "wasm")]
    if let Some(ref engine) = engine {
        engine.on_downstream_close_with_modules(wasm_modules).await;
        engine.on_upstream_close_with_modules(wasm_modules).await;
    }
}

/// TLS 終端モードの接続を処理する（ハンドシェイク → upstream 接続 → 平文転送）。
async fn handle_l4_tls_terminate_connection(
    client: IoUringTcpStream,
    peer_addr: SocketAddr,
    config: Arc<L4ListenerConfig>,
    upstream_targets: Arc<Vec<L4UpstreamTarget>>,
    rr_state: Arc<RoundRobinState>,
    conn_counters: Arc<Vec<AtomicUsize>>,
    health_state: Arc<Vec<AtomicBool>>,
) {
    let handshake_timeout = Duration::from_secs(config.connect_timeout_secs);
    let Some(mut tls_client) =
        accept_l4_tls_client(client, handshake_timeout, &config.name, peer_addr).await
    else {
        return;
    };

    let (upstream_idx, upstream_addr_str) =
        match select_upstream(&config, &rr_state, &conn_counters, &health_state) {
            Some(pair) => pair,
            None => {
                warn!("[L4:{}] no healthy upstream available", config.name);
                return;
            }
        };

    if let Some(c) = conn_counters.get(upstream_idx) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    struct UpstreamGuard {
        counters: Arc<Vec<AtomicUsize>>,
        idx: usize,
    }
    impl Drop for UpstreamGuard {
        fn drop(&mut self) {
            if let Some(c) = self.counters.get(self.idx) {
                c.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
    let _upstream_guard = UpstreamGuard {
        counters: conn_counters.clone(),
        idx: upstream_idx,
    };

    let endpoint = match upstream_targets.get(upstream_idx) {
        Some(target) => match resolve_upstream_target(target).await {
            Some(a) => a,
            None => {
                warn!(
                    "[L4:{}] failed to resolve upstream {}",
                    config.name, upstream_addr_str
                );
                return;
            }
        },
        None => {
            warn!(
                "[L4:{}] upstream_targets index {} out of range",
                config.name, upstream_idx
            );
            return;
        }
    };

    let connect_timeout = Duration::from_secs(config.connect_timeout_secs);
    let upstream = match timeout(connect_timeout, connect_l4_endpoint(&endpoint)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            warn!(
                "[L4:{}] failed to connect to upstream {}: {}",
                config.name, upstream_addr_str, e
            );
            return;
        }
        Err(_) => {
            warn!(
                "[L4:{}] connection to upstream {} timed out",
                config.name, upstream_addr_str
            );
            return;
        }
    };
    let _ = upstream.set_nodelay(true);

    info!(
        "[L4:{}] {} → {} (tls=Terminate)",
        config.name, peer_addr, upstream_addr_str
    );

    let idle_timeout = Duration::from_secs(config.idle_timeout_secs);
    bidirectional_forward_tls_terminate(
        &mut tls_client,
        &upstream,
        idle_timeout,
        &config.name,
        #[cfg(feature = "wasm")]
        &config.resolved_wasm_modules,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{L4LbAlgorithm, L4ListenerConfig, L4TlsMode, L4UpstreamEntry};

    fn make_config_weighted(upstreams: Vec<(&str, u32)>, lb: L4LbAlgorithm) -> L4ListenerConfig {
        L4ListenerConfig {
            name: "test".to_string(),
            listen: "0.0.0.0:9999".to_string(),
            upstreams: upstreams
                .into_iter()
                .map(|(a, w)| L4UpstreamEntry {
                    addr: a.to_string(),
                    weight: w,
                })
                .collect(),
            protocol: crate::config::L4Protocol::Tcp,
            lb,
            tls: L4TlsMode::None,
            max_connections: 0,
            health_check: None,
            connect_timeout_secs: 10,
            idle_timeout_secs: 600,
            #[cfg(feature = "wasm")]
            wasm_modules: Vec::new(),
            #[cfg(feature = "wasm")]
            module_configuration: None,
            #[cfg(feature = "wasm")]
            resolved_wasm_modules: std::sync::Arc::new(Vec::new()),
        }
    }

    fn make_config(upstreams: Vec<&str>, lb: L4LbAlgorithm) -> L4ListenerConfig {
        make_config_weighted(upstreams.into_iter().map(|a| (a, 1u32)).collect(), lb)
    }

    fn all_healthy(n: usize) -> Vec<AtomicBool> {
        (0..n).map(|_| AtomicBool::new(true)).collect()
    }

    #[test]
    fn test_round_robin_selection() {
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"],
            L4LbAlgorithm::RoundRobin,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(0)).collect();
        let health = all_healthy(3);

        let (_, a0) = select_upstream(&config, &rr, &counters, &health).unwrap();
        let (_, a1) = select_upstream(&config, &rr, &counters, &health).unwrap();
        let (_, a2) = select_upstream(&config, &rr, &counters, &health).unwrap();
        let (_, a3) = select_upstream(&config, &rr, &counters, &health).unwrap();

        assert_eq!(a0, "127.0.0.1:8001");
        assert_eq!(a1, "127.0.0.1:8002");
        assert_eq!(a2, "127.0.0.1:8003");
        assert_eq!(a3, "127.0.0.1:8001");
    }

    #[test]
    fn test_least_conn_selection() {
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002"],
            L4LbAlgorithm::LeastConn,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = vec![AtomicUsize::new(5), AtomicUsize::new(2)];
        let health = all_healthy(2);

        let (idx, addr) = select_upstream(&config, &rr, &counters, &health).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(addr, "127.0.0.1:8002");
    }

    #[test]
    fn test_select_upstream_empty() {
        let config = make_config(vec![], L4LbAlgorithm::RoundRobin);
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = vec![];
        let health: Vec<AtomicBool> = vec![];

        assert!(select_upstream(&config, &rr, &counters, &health).is_none());
    }

    #[test]
    fn test_parse_upstream_addrs_valid() {
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002"],
            L4LbAlgorithm::RoundRobin,
        );
        let addrs = parse_upstream_addrs(&config).unwrap();
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], "127.0.0.1:8001".parse::<SocketAddr>().unwrap());
        assert_eq!(addrs[1], "127.0.0.1:8002".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn test_parse_upstream_addrs_invalid() {
        let config = make_config(vec!["not-a-valid-addr"], L4LbAlgorithm::RoundRobin);
        assert!(parse_upstream_addrs(&config).is_err());
    }

    #[test]
    fn test_parse_upstream_targets_unresolved_hostname() {
        let config = make_config(vec!["backend.example:4443"], L4LbAlgorithm::RoundRobin);
        let targets = parse_upstream_targets(&config);
        assert_eq!(targets.len(), 1);
        match &targets[0] {
            L4UpstreamTarget::Unresolved(host) => assert_eq!(host.as_ref(), "backend.example:4443"),
            other => panic!("expected Unresolved for hostname upstream, got {:?}", other),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_upstream_targets_unix_socket() {
        let config = make_config(vec!["unix:/run/db.sock"], L4LbAlgorithm::RoundRobin);
        let targets = parse_upstream_targets(&config);
        assert_eq!(targets.len(), 1);
        match &targets[0] {
            L4UpstreamTarget::Unix(path) => assert_eq!(path.as_ref(), "/run/db.sock"),
            other => panic!("expected Unix for unix: upstream, got {:?}", other),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_upstream_addrs_rejects_unix_socket() {
        // parse_upstream_addrs は SocketAddr のみを返すため UDS はエラーになる。
        let config = make_config(vec!["unix:/run/db.sock"], L4LbAlgorithm::RoundRobin);
        assert!(parse_upstream_addrs(&config).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_resolve_upstream_target_unix() {
        let target = L4UpstreamTarget::Unix(Arc::from("/run/db.sock"));
        let endpoint = resolve_upstream_target(&target).await;
        match endpoint {
            Some(L4Endpoint::Unix(path)) => assert_eq!(path.as_ref(), "/run/db.sock"),
            other => panic!("expected L4Endpoint::Unix, got {:?}", other),
        }
    }

    #[test]
    fn test_connection_counter() {
        let counter = L4ConnectionCounter::new();
        counter.current.fetch_add(1, Ordering::Relaxed);
        counter.current.fetch_add(1, Ordering::Relaxed);
        counter.total.fetch_add(2, Ordering::Relaxed);

        assert_eq!(counter.current.load(Ordering::Relaxed), 2);
        assert_eq!(counter.total.load(Ordering::Relaxed), 2);

        counter.current.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(counter.current.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_max_connections_zero_means_unlimited() {
        let config = make_config(vec!["127.0.0.1:8001"], L4LbAlgorithm::RoundRobin);
        assert_eq!(config.max_connections, 0);
    }

    #[test]
    fn test_round_robin_wrap() {
        let rr = RoundRobinState::new();
        for i in 0..10usize {
            assert_eq!(rr.next(3), i % 3);
        }
    }

    #[test]
    fn test_round_robin_single_upstream() {
        let config = make_config(vec!["127.0.0.1:8001"], L4LbAlgorithm::RoundRobin);
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = vec![AtomicUsize::new(0)];
        let health = all_healthy(1);

        for _ in 0..5 {
            let (idx, addr) = select_upstream(&config, &rr, &counters, &health).unwrap();
            assert_eq!(idx, 0);
            assert_eq!(addr, "127.0.0.1:8001");
        }
    }

    #[test]
    fn test_least_conn_single_upstream() {
        let config = make_config(vec!["127.0.0.1:8001"], L4LbAlgorithm::LeastConn);
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = vec![AtomicUsize::new(10)];
        let health = all_healthy(1);

        let (idx, addr) = select_upstream(&config, &rr, &counters, &health).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(addr, "127.0.0.1:8001");
    }

    #[test]
    fn test_least_conn_all_equal() {
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"],
            L4LbAlgorithm::LeastConn,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(3)).collect();
        let health = all_healthy(3);

        let (idx, addr) = select_upstream(&config, &rr, &counters, &health).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(
            addr, "127.0.0.1:8001",
            "ties should resolve to first upstream"
        );
    }

    #[test]
    fn test_max_connections_zero_is_unlimited() {
        let config = make_config(vec!["127.0.0.1:8001"], L4LbAlgorithm::RoundRobin);
        assert_eq!(config.max_connections, 0);
        let counter = L4ConnectionCounter::new();
        counter.current.store(9999, Ordering::Relaxed);
        assert!(
            !(config.max_connections > 0
                && counter.current.load(Ordering::Relaxed) >= config.max_connections),
            "max_connections=0 should never trigger limit"
        );
    }

    #[test]
    fn test_max_connections_limit_logic() {
        let mut config = make_config(vec!["127.0.0.1:8001"], L4LbAlgorithm::RoundRobin);
        config.max_connections = 5;
        let counter = L4ConnectionCounter::new();
        counter.current.store(5, Ordering::Relaxed);

        let should_reject = config.max_connections > 0
            && counter.current.load(Ordering::Relaxed) >= config.max_connections;
        assert!(should_reject, "current == max should trigger limit");

        counter.current.store(4, Ordering::Relaxed);
        let should_reject2 = config.max_connections > 0
            && counter.current.load(Ordering::Relaxed) >= config.max_connections;
        assert!(!should_reject2, "current < max should not trigger limit");
    }

    #[test]
    fn test_counter_guard_decrements_on_drop() {
        let counter = Arc::new(L4ConnectionCounter::new());
        counter.current.store(1, Ordering::Relaxed);

        struct CounterGuard(Arc<L4ConnectionCounter>);
        impl Drop for CounterGuard {
            fn drop(&mut self) {
                self.0.current.fetch_sub(1, Ordering::Relaxed);
            }
        }

        {
            let _guard = CounterGuard(counter.clone());
            assert_eq!(counter.current.load(Ordering::Relaxed), 1);
        }
        assert_eq!(
            counter.current.load(Ordering::Relaxed),
            0,
            "drop should decrement"
        );
    }

    #[test]
    fn test_least_conn_picks_minimum() {
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"],
            L4LbAlgorithm::LeastConn,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = vec![
            AtomicUsize::new(10),
            AtomicUsize::new(1),
            AtomicUsize::new(5),
        ];
        let health = all_healthy(3);

        let (idx, addr) = select_upstream(&config, &rr, &counters, &health).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(
            addr, "127.0.0.1:8002",
            "should pick upstream with fewest connections"
        );
    }

    #[test]
    fn test_select_upstream_skips_unhealthy() {
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002"],
            L4LbAlgorithm::RoundRobin,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..2).map(|_| AtomicUsize::new(0)).collect();
        let health = vec![AtomicBool::new(false), AtomicBool::new(true)];

        for _ in 0..5 {
            let (idx, addr) = select_upstream(&config, &rr, &counters, &health).unwrap();
            assert_eq!(idx, 1, "unhealthy upstream[0] should be skipped");
            assert_eq!(addr, "127.0.0.1:8002");
        }
    }

    #[test]
    fn test_select_upstream_all_unhealthy_returns_none() {
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002"],
            L4LbAlgorithm::RoundRobin,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..2).map(|_| AtomicUsize::new(0)).collect();
        let health = vec![AtomicBool::new(false), AtomicBool::new(false)];

        assert!(select_upstream(&config, &rr, &counters, &health).is_none());
    }

    #[test]
    fn test_least_conn_skips_unhealthy() {
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"],
            L4LbAlgorithm::LeastConn,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(0)).collect();
        let health = vec![
            AtomicBool::new(false),
            AtomicBool::new(true),
            AtomicBool::new(true),
        ];

        let (idx, _) = select_upstream(&config, &rr, &counters, &health).unwrap();
        assert_ne!(idx, 0, "unhealthy upstream[0] should be skipped");
    }

    #[test]
    fn test_weighted_round_robin_distribution() {
        let config = make_config_weighted(
            vec![("127.0.0.1:8001", 2), ("127.0.0.1:8002", 1)],
            L4LbAlgorithm::RoundRobin,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..2).map(|_| AtomicUsize::new(0)).collect();
        let health = all_healthy(2);

        let results: Vec<usize> = (0..3)
            .map(|_| select_upstream(&config, &rr, &counters, &health).unwrap().0)
            .collect();

        let count_0 = results.iter().filter(|&&i| i == 0).count();
        let count_1 = results.iter().filter(|&&i| i == 1).count();
        assert_eq!(count_0, 2, "upstream[0] should get 2/3 of requests");
        assert_eq!(count_1, 1, "upstream[1] should get 1/3 of requests");
    }

    #[test]
    fn test_weighted_round_robin_equal_weight_is_round_robin() {
        let config = make_config_weighted(
            vec![
                ("127.0.0.1:8001", 1),
                ("127.0.0.1:8002", 1),
                ("127.0.0.1:8003", 1),
            ],
            L4LbAlgorithm::RoundRobin,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(0)).collect();
        let health = all_healthy(3);

        let results: Vec<usize> = (0..6)
            .map(|_| select_upstream(&config, &rr, &counters, &health).unwrap().0)
            .collect();

        for expected in 0..3 {
            let count = results.iter().filter(|&&i| i == expected).count();
            assert_eq!(
                count, 2,
                "upstream[{}] should appear twice in 6 calls",
                expected
            );
        }
    }

    #[test]
    fn test_weighted_rr_skips_unhealthy_in_weight_calculation() {
        let config = make_config_weighted(
            vec![("127.0.0.1:8001", 2), ("127.0.0.1:8002", 1)],
            L4LbAlgorithm::RoundRobin,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..2).map(|_| AtomicUsize::new(0)).collect();
        let health = vec![AtomicBool::new(false), AtomicBool::new(true)];

        for _ in 0..5 {
            let (idx, _) = select_upstream(&config, &rr, &counters, &health).unwrap();
            assert_eq!(idx, 1, "only healthy upstream[1] should be selected");
        }
    }

    #[test]
    fn test_least_conn_uses_per_upstream_counters() {
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002"],
            L4LbAlgorithm::LeastConn,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..2).map(|_| AtomicUsize::new(0)).collect();
        let health = all_healthy(2);

        let (idx1, _) = select_upstream(&config, &rr, &counters, &health).unwrap();
        counters[idx1].fetch_add(1, Ordering::Relaxed);

        let (idx2, _) = select_upstream(&config, &rr, &counters, &health).unwrap();
        assert_eq!(
            idx2, 1,
            "after incrementing idx1 counter, idx2 should be the other"
        );
        counters[idx2].fetch_add(1, Ordering::Relaxed);

        let (idx3, _) = select_upstream(&config, &rr, &counters, &health).unwrap();
        assert_eq!(idx3, 0, "tie should resolve to first upstream");
    }

    #[test]
    fn test_idle_timeout_config() {
        let mut config = make_config(vec!["127.0.0.1:8001"], L4LbAlgorithm::RoundRobin);
        config.idle_timeout_secs = 30;
        assert_eq!(config.idle_timeout_secs, 30);
    }

    // ====================
    // forward_direction のバッファ再利用テスト
    // ====================

    #[test]
    fn test_buf_size_is_64k() {
        // BUF_SIZE が 64KB であることを確認（変更時にこのテストが警告する）
        assert_eq!(BUF_SIZE, 64 * 1024);
    }

    #[test]
    fn test_copy_within_simulates_short_write_retry() {
        // forward_direction のショートライト処理（copy_within）を単体でテスト
        let mut buf: Vec<u8> = vec![1, 2, 3, 4, 5, 6];
        let n = 6;
        let written = 2; // 2 バイトだけ書けた

        buf.copy_within(written..n, 0);
        unsafe { buf.set_len(n - written) };

        // [3, 4, 5, 6] が先頭に詰められているはず
        assert_eq!(&buf[..], &[3u8, 4, 5, 6]);
    }

    // ====================
    // L4 splice パイプのスレッドローカルプール（F-40）
    // ====================

    /// 空のパイプは返却後に再利用される（同じ fd が返る = pipe2(2) が発行されない）。
    #[cfg(target_os = "linux")]
    #[test]
    fn test_pipe_pool_reuses_clean_pipe() {
        let pipe = acquire_pipe().expect("pipe");
        let (rfd, wfd) = (pipe.read_fd, pipe.write_fd);
        release_pipe(pipe);

        let reused = acquire_pipe().expect("pipe");
        assert_eq!(
            (reused.read_fd, reused.write_fd),
            (rfd, wfd),
            "clean pipe must be reused from the pool"
        );
        release_pipe(reused);
    }

    /// 残データのあるパイプは返却時に破棄され、次の接続へデータが混線しない。
    #[cfg(target_os = "linux")]
    #[test]
    fn test_pipe_pool_discards_dirty_pipe() {
        // プールを空にしてから開始（他テストの影響を排除）
        L4_PIPE_POOL.with(|p| p.borrow_mut().clear());

        let pipe = acquire_pipe().expect("pipe");
        // パイプに残データを作る
        let data = b"leftover";
        let n = unsafe {
            libc::write(
                pipe.write_fd,
                data.as_ptr() as *const libc::c_void,
                data.len(),
            )
        };
        assert_eq!(n, data.len() as isize);

        release_pipe(pipe);

        // 汚れたパイプは破棄されたので、プールは空のまま
        let pooled = L4_PIPE_POOL.with(|p| p.borrow().len());
        assert_eq!(pooled, 0, "dirty pipe must be discarded, not pooled");

        // 次に取得したパイプは空である（データ混線なし）
        let fresh = acquire_pipe().expect("pipe");
        let mut pending: libc::c_int = -1;
        let ret = unsafe { libc::ioctl(fresh.read_fd, libc::FIONREAD, &mut pending) };
        assert_eq!(ret, 0);
        assert_eq!(pending, 0, "freshly acquired pipe must be empty");
        release_pipe(fresh);
    }

    /// プール上限を超えた返却は破棄される（fd リークなし・無制限成長なし）。
    #[cfg(target_os = "linux")]
    #[test]
    fn test_pipe_pool_respects_max() {
        L4_PIPE_POOL.with(|p| p.borrow_mut().clear());

        let pipes: Vec<Pipe> = (0..L4_PIPE_POOL_MAX + 8)
            .map(|_| Pipe::new().expect("pipe"))
            .collect();
        for pipe in pipes {
            release_pipe(pipe);
        }
        let pooled = L4_PIPE_POOL.with(|p| p.borrow().len());
        assert_eq!(pooled, L4_PIPE_POOL_MAX);
    }

    // ====================
    // B-45: 半クローズ伝搬の回帰テスト
    // ====================
    //
    // io_uring/epoll を許可しない環境（Docker ビルドサンドボックス・古いカーネル等）では
    // ランタイムドライバの生成が失敗するため、実 I/O を伴う本テストはスキップする
    // （E2E で網羅）。
    // `forward_direction_splice`/`Pipe` が Linux 専用のため、本ヘルパーも呼び出し元の
    // テストと同じく Linux 限定にする（`veil_rt_reactor` は kqueue（FreeBSD）でも
    // 立つため、`target_os = "linux"` を明示しないと `libc::epoll_create1` が
    // FreeBSD ビルドでコンパイルエラーになる）。
    #[cfg(all(veil_rt_uring, target_os = "linux"))]
    fn io_uring_available() -> bool {
        crate::runtime::ring::IoUring::new(8, 0).is_ok()
    }

    /// reactor（epoll）ビルドでは `epoll_create1` の成否をランタイム可用性の代替指標とする。
    #[cfg(all(veil_rt_reactor, target_os = "linux"))]
    fn io_uring_available() -> bool {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd >= 0 {
            unsafe { libc::close(fd) };
            true
        } else {
            false
        }
    }

    /// `forward_direction_splice` が src の EOF でループを抜けた際、dst へ
    /// `shutdown(SHUT_WR)` を発行して半クローズ（FIN）を対向へ伝搬すること。
    ///
    /// 修正前は src の EOF で自方向のみ終了し dst には何も伝搬しなかったため、
    /// 対向側（dst の実体を保持するピア）はデータも FIN も受け取れず、
    /// アイドルタイムアウトまで fd を保持し続けていた（B-45）。
    ///
    /// `forward_direction_splice`/`Pipe` は Linux 専用（splice(2)）のため本テストも
    /// Linux 限定。BSD の半クローズ伝搬（`forward_direction` 経由）は E2E で確認する。
    #[cfg(target_os = "linux")]
    #[test]
    fn test_forward_direction_splice_propagates_half_close_on_src_eof() {
        if !io_uring_available() {
            eprintln!(
                "io_uring unavailable; skipping test_forward_direction_splice_propagates_half_close_on_src_eof"
            );
            return;
        }

        // src 側: accept 直後に書き込み方向を閉じ、こちら側の TcpStream に EOF を送る。
        let src_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind src");
        let src_addr = src_listener.local_addr().expect("src addr");
        let src_peer_handle = std::thread::spawn(move || {
            let (peer, _) = src_listener.accept().expect("accept src peer");
            peer.shutdown(std::net::Shutdown::Write)
                .expect("shutdown write");
            // スレッド終了までピアを保持し、テスト側の shutdown(SHUT_WR) を観測できるようにする。
            // 別 OS スレッド上のテストコードでイベントループはブロックしないため許容。
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(Duration::from_millis(500));
        });

        // dst 側: accept 後、read() が 0 (EOF) を返すまで待つ（FIN 到達の検知）。
        let dst_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind dst");
        let dst_addr = dst_listener.local_addr().expect("dst addr");
        let (eof_tx, eof_rx) = std::sync::mpsc::channel::<()>();
        let dst_peer_handle = std::thread::spawn(move || {
            use std::io::Read;
            let (mut peer, _) = dst_listener.accept().expect("accept dst peer");
            let mut buf = [0u8; 16];
            loop {
                match peer.read(&mut buf) {
                    Ok(0) => {
                        let _ = eof_tx.send(());
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        });

        crate::runtime::block_on(async move {
            let src = IoUringTcpStream::connect(src_addr)
                .await
                .expect("connect src");
            let dst = IoUringTcpStream::connect(dst_addr)
                .await
                .expect("connect dst");
            let pipe = Pipe::new().expect("pipe");

            // idle_timeout はあえて長め（30秒）にし、EOF 検知が
            // タイムアウト待ちではなく即座の shutdown 伝搬によることを保証する。
            let total =
                forward_direction_splice(&src, &dst, &pipe, Duration::from_secs(30), "test-b45")
                    .await;
            assert_eq!(total, 0, "no data was transferred before src EOF");
        });

        // dst 側ピアが FIN を受け取ったことを短時間（idle_timeout の 30 秒よりずっと短い）で確認する。
        eof_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("dst peer must observe EOF promptly (half-close propagation)");

        src_peer_handle.join().expect("join src peer thread");
        dst_peer_handle.join().expect("join dst peer thread");
    }

    // ====================
    // B-57: WASM close_stream の方向間キャンセル共有の回帰テスト
    // ====================
    //
    // `forward_direction_wasm` は `wasm` feature 有効時のみ存在し、target_os 非依存
    // （splice と違い Linux 限定ではない）。ただし実 I/O を伴うため、ランタイム
    // ドライバ（io_uring/epoll/kqueue）が使えない環境ではスキップする。
    #[cfg(feature = "wasm")]
    #[cfg(veil_rt_uring)]
    fn wasm_forward_runtime_available() -> bool {
        crate::runtime::ring::IoUring::new(8, 0).is_ok()
    }

    #[cfg(feature = "wasm")]
    #[cfg(all(veil_rt_reactor, target_os = "linux"))]
    fn wasm_forward_runtime_available() -> bool {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd >= 0 {
            unsafe { libc::close(fd) };
            true
        } else {
            false
        }
    }

    /// BSD/macOS の kqueue readiness reactor は（io_uring と違って）テスト環境で
    /// 権限制約により使えないケースが実質無いため、無条件で利用可能とみなす。
    #[cfg(feature = "wasm")]
    #[cfg(all(veil_rt_reactor, not(target_os = "linux")))]
    fn wasm_forward_runtime_available() -> bool {
        true
    }

    /// `close_requested` が既に立っている状態で `forward_direction_wasm` を呼ぶと、
    /// WASM エンジン呼び出しにすら到達せず（ループ先頭のアトミック load 1 回のみで）
    /// 即座に転送を止めること。
    ///
    /// 修正前は `forward_direction_wasm` に方向間の共有状態が存在せず、対向方向が
    /// close 済みでも自分は読み取ったデータをそのまま転送し続けていた（B-57）。
    /// この経路は接続確立時に既に読み取り可能なデータを src 側へ用意しておき、
    /// 修正後の実装がそのデータへ一切触れず（read すら発行せず）即座に戻ることを検証する。
    #[cfg(feature = "wasm")]
    #[test]
    fn test_forward_direction_wasm_stops_before_read_when_close_already_requested() {
        if !wasm_forward_runtime_available() {
            eprintln!(
                "runtime unavailable; skipping test_forward_direction_wasm_stops_before_read_when_close_already_requested"
            );
            return;
        }

        // src 側: 接続直後に「転送されてはいけないデータ」を書き込んでおく。
        let src_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind src");
        let src_addr = src_listener.local_addr().expect("src addr");
        let src_peer_handle = std::thread::spawn(move || {
            use std::io::Write;
            let (mut peer, _) = src_listener.accept().expect("accept src peer");
            peer.write_all(b"must-not-be-forwarded")
                .expect("write src data");
            // テスト側の block_on が完了するまでピアを保持する。
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(Duration::from_millis(300));
        });

        // dst 側: 何か 1 バイトでも受信したかどうかだけを報告する。
        let dst_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind dst");
        let dst_addr = dst_listener.local_addr().expect("dst addr");
        let (data_tx, data_rx) = std::sync::mpsc::channel::<usize>();
        let dst_peer_handle = std::thread::spawn(move || {
            use std::io::Read;
            let (mut peer, _) = dst_listener.accept().expect("accept dst peer");
            peer.set_read_timeout(Some(Duration::from_millis(500)))
                .expect("set_read_timeout");
            let mut buf = [0u8; 64];
            let n = peer.read(&mut buf).unwrap_or(0);
            let _ = data_tx.send(n);
        });

        let close_requested = Arc::new(AtomicBool::new(true));

        crate::runtime::block_on(async move {
            let src = IoUringTcpStream::connect(src_addr)
                .await
                .expect("connect src");
            let dst = IoUringTcpStream::connect(dst_addr)
                .await
                .expect("connect dst");

            let start = std::time::Instant::now();
            // idle_timeout はあえて長め（30秒）にし、早期リターンがタイムアウト待ちに
            // よるものではなく close_requested チェックそのものによることを保証する。
            let total = forward_direction_wasm(
                &src,
                &dst,
                Duration::from_secs(30),
                "test-b57-pre-close",
                &[],
                true,
                &close_requested,
            )
            .await;
            assert_eq!(
                total, 0,
                "no bytes should be forwarded once close is already requested"
            );
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "close_requested check must short-circuit long before idle_timeout"
            );
        });

        let received = data_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("dst peer thread must report a result");
        assert_eq!(
            received, 0,
            "dst must never receive forwarded data once close is requested (B-57)"
        );

        src_peer_handle.join().expect("join src peer thread");
        dst_peer_handle.join().expect("join dst peer thread");
    }

    /// `bidirectional_forward` の 2 方向は `(src, dst) = (client, upstream)` と
    /// `(upstream, client)` のように src/dst が入れ替わって同じソケットを共有する。
    /// 片方向が close する際に自分の `src`/`dst` 双方へ `shutdown(Shutdown::Both)`
    /// を発行する設計（本チケットの修正）が、対向方向がブロックしている `read` を
    /// アイドルタイムアウトを待たずに即座に起こすことを検証する。
    ///
    /// WASM モジュール/エンジンは使わず、`NetworkFilterResult::Close` が実際に
    /// 行うのと同じ shutdown 操作を外部から模して行う。エンジンを介した実際の
    /// close 分岐（マーカー検出→ Close 判定）自体は `tests/e2e_tests.rs` の
    /// `test_l4_wasm_close_on_marker`（ライブソケット越しの E2E）でのみ検証可能なため、
    /// ここでは意図的にそちらへ委ねる。
    #[cfg(feature = "wasm")]
    #[test]
    fn test_forward_direction_wasm_shutdown_unblocks_peers_blocked_read() {
        if !wasm_forward_runtime_available() {
            eprintln!(
                "runtime unavailable; skipping test_forward_direction_wasm_shutdown_unblocks_peers_blocked_read"
            );
            return;
        }

        // ピアは「テスト側から明示的に合図されるまで」接続を保持する（合図は
        // block_on 完了直後に送信元 channel を drop することで行う）。固定 sleep
        // ではなくこの方式にすることで、(1) 万一 shutdown 伝搬が効かず read が
        // idle_timeout いっぱいブロックしても確実にそれより長く生き残り「対向の
        // 自然切断」による偽陽性を防ぎつつ、(2) 正常系（修正が効いて数百ms で
        // 完了する場合）はピアを即座に解放してテストを高速に保てる。
        // フォールバックの 20 秒は、本テストで使う idle_timeout（30 秒未満）より
        // 短く見えるが、合図が正しく送られる限り実際に使われることはない
        // （合図が届かない異常系のみのセーフティネット）。
        let (client_done_tx, client_done_rx) = std::sync::mpsc::channel::<()>();
        let client_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind client");
        let client_addr = client_listener.local_addr().expect("client addr");
        let client_peer_handle = std::thread::spawn(move || {
            let (peer, _) = client_listener.accept().expect("accept client peer");
            // 何も送らずピアを保持する（B 方向の書き込み先）。
            let _ = client_done_rx.recv_timeout(Duration::from_secs(20));
            drop(peer);
        });

        let (upstream_done_tx, upstream_done_rx) = std::sync::mpsc::channel::<()>();
        let upstream_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        let upstream_peer_handle = std::thread::spawn(move || {
            let (peer, _) = upstream_listener.accept().expect("accept upstream peer");
            // データを送らず保持し、B 方向の read が実際にブロックする状況を作る。
            let _ = upstream_done_rx.recv_timeout(Duration::from_secs(20));
            drop(peer);
        });

        let close_requested = Arc::new(AtomicBool::new(false));

        crate::runtime::block_on(async move {
            let client = IoUringTcpStream::connect(client_addr)
                .await
                .expect("connect client");
            let upstream = IoUringTcpStream::connect(upstream_addr)
                .await
                .expect("connect upstream");

            let start = std::time::Instant::now();

            // B 方向（upstream → client）。production と同じ引数の並び。idle_timeout は
            // あえて長め（30秒）にし、早期リターンがタイムアウト待ちではなく
            // shutdown(Both) による即時の read 解除によることを保証する。
            let b_direction = forward_direction_wasm(
                &upstream,
                &client,
                Duration::from_secs(30),
                "test-b57-b",
                &[],
                false,
                &close_requested,
            );

            // A 方向が Close を検知した際に実行するのと同じ操作
            // （`close_requested` を立てて自分の src/dst 双方を shutdown(Both)）を、
            // 100ms 後に外部から模す。A の src/dst はちょうど B の dst/src と同一
            // ソケットのため、この 2 回の shutdown で B の read が起きるはずである。
            let closer = async {
                crate::runtime::time::sleep(Duration::from_millis(100)).await;
                close_requested.store(true, Ordering::SeqCst);
                let _ = client.shutdown(std::net::Shutdown::Both);
                let _ = upstream.shutdown(std::net::Shutdown::Both);
            };

            let (total, _) = futures::join!(b_direction, closer);
            assert_eq!(total, 0, "no data should have been forwarded");
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "shutdown(Both) must unblock the peer's blocked read promptly instead of \
                 waiting for idle_timeout (B-57)"
            );
        });

        // ピアへ「もう閉じてよい」と合図してから join する（正常系は即座に解放される）。
        drop(client_done_tx);
        drop(upstream_done_tx);
        client_peer_handle.join().expect("join client peer thread");
        upstream_peer_handle
            .join()
            .expect("join upstream peer thread");
    }
}
