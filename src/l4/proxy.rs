//! L4 TCP プロキシ実装
//!
//! バイダイレクショナルストリーム転送、ロードバランシング、TLS パススルーを実装する。

use crate::config::{L4LbAlgorithm, L4ListenerConfig, L4TlsMode};
use crate::runtime::splice::{splice, Pipe};
use crate::runtime::tcp::TcpStream as IoUringTcpStream;
use crate::runtime::time::timeout;
use ftlog::{debug, info, warn};
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::io::AsRawFd;
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

/// 設定ファイルのアドレス文字列を起動時に `SocketAddr` へ変換して保持する。
///
/// hot path での DNS 解決（`to_socket_addrs` は blocking syscall）を排除するため、
/// サーバ起動時に一度だけパースしてキャッシュする。
pub fn parse_upstream_addrs(config: &L4ListenerConfig) -> Result<Vec<SocketAddr>, String> {
    config
        .upstreams
        .iter()
        .map(|u| {
            u.addr
                .to_socket_addrs()
                .map_err(|e| format!("failed to parse upstream addr '{}': {}", u.addr, e))?
                .next()
                .ok_or_else(|| format!("no address resolved for '{}'", u.addr))
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
                        .map_or(true, |h| h.load(Ordering::Relaxed))
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
                    .map_or(true, |h| h.load(Ordering::Relaxed));
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
                    .map_or(true, |h| h.load(Ordering::Relaxed))
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
                    .map_or(true, |h| h.load(Ordering::Relaxed))
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
    let mut buf: Vec<u8> = Vec::with_capacity(BUF_SIZE);
    // SAFETY: capacity ぶんの領域は確保済み。カーネルが read で上書きするため
    // Rust 側から [len..capacity] を読まない限り安全。
    unsafe { buf.set_len(BUF_SIZE) };

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
    total
}

/// 1 方向の splice ゼロコピー転送ループ（F-30）。
///
/// `src(socket) → pipe → dst(socket)` の 2 段 splice でカーネル内転送する。ユーザースペースの
/// バッファを一切経由しない（メモリコピー・ヒープ確保なし）。`readable()`/`writable()`
/// （POLL_ADD）で待機し、ノンブロッキング splice をドレインループで回す（エッジトリガ）。
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
) {
    // futures::join! は両 Future を同一タスク内でインターリーブするため、
    // &TcpStream / &Pipe の同時借用は安全。
    match (Pipe::new(), Pipe::new()) {
        (Ok(c2u_pipe), Ok(u2c_pipe)) => {
            let (c2u_bytes, u2c_bytes) = futures::join!(
                forward_direction_splice(
                    &client,
                    &upstream,
                    &c2u_pipe,
                    idle_timeout,
                    listener_name
                ),
                forward_direction_splice(
                    &upstream,
                    &client,
                    &u2c_pipe,
                    idle_timeout,
                    listener_name
                )
            );
            debug!(
                "[L4:{}] connection closed (splice): c→u {} bytes, u→c {} bytes",
                listener_name, c2u_bytes, u2c_bytes
            );
        }
        _ => {
            // pipe 作成失敗時はユーザースペースコピーへフォールバック。
            warn!(
                "[L4:{}] pipe creation failed; falling back to userspace copy",
                listener_name
            );
            let (c2u_bytes, u2c_bytes) = futures::join!(
                forward_direction(&client, &upstream, idle_timeout, listener_name),
                forward_direction(&upstream, &client, idle_timeout, listener_name)
            );
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
    parsed_addrs: Arc<Vec<SocketAddr>>,
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
        warn!(
            "[L4:{}] TLS termination not yet implemented, treating as passthrough",
            config.name
        );
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

    // 起動時パース済み SocketAddr を使用（hot path での DNS 解決を排除）
    let socket_addr = match parsed_addrs.get(upstream_idx) {
        Some(a) => *a,
        None => {
            warn!(
                "[L4:{}] parsed_addrs index {} out of range",
                config.name, upstream_idx
            );
            return;
        }
    };

    let connect_timeout = Duration::from_secs(config.connect_timeout_secs);
    let upstream = match timeout(
        connect_timeout,
        IoUringTcpStream::connect(socket_addr), // SocketAddr → blocking DNS なし
    )
    .await
    {
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
    bidirectional_forward(client, upstream, idle_timeout, &config.name).await;
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
            lb,
            tls: L4TlsMode::None,
            max_connections: 0,
            health_check: None,
            connect_timeout_secs: 10,
            idle_timeout_secs: 600,
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
}
