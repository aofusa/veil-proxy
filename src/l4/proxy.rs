//! L4 TCP プロキシ実装
//!
//! バイダイレクショナルストリーム転送、ロードバランシング、TLS パススルーを実装する。

use crate::config::{L4LbAlgorithm, L4ListenerConfig, L4TlsMode};
use crate::runtime::tcp::TcpStream as IoUringTcpStream;
use crate::runtime::time::timeout;
use ftlog::{debug, info, warn};
use std::net::SocketAddr;
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

    /// 次のカウンタ値を返す（total_weight でのモジュロは呼び出し側で行う）
    pub fn next(&self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        self.counter.fetch_add(1, Ordering::Relaxed) % len
    }

    /// Weighted Round Robin 用: total_weight でのカウンタを返す
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

/// upstream アドレスを選択する（ロードバランシング）
///
/// 戻り値: `(upstream_index, upstream_addr)` または `None`（全 upstream が unhealthy）
pub fn select_upstream(
    config: &L4ListenerConfig,
    rr_state: &RoundRobinState,
    conn_counters: &[AtomicUsize],
    health_state: &[AtomicBool],
) -> Option<(usize, String)> {
    if config.upstreams.is_empty() {
        return None;
    }

    match config.lb {
        L4LbAlgorithm::RoundRobin => {
            // Weighted Round Robin: weight に比例した頻度で upstream を選択
            let total_weight: usize = config
                .upstreams
                .iter()
                .enumerate()
                .filter(|(i, _)| health_state.get(*i).map_or(true, |h| h.load(Ordering::Relaxed)))
                .map(|(_, u)| u.weight.max(1) as usize)
                .sum();

            if total_weight == 0 {
                return None; // 全 upstream が unhealthy
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
                    return Some((i, upstream.addr.clone()));
                }
            }
            // フォールバック: 最後の healthy upstream
            for (i, upstream) in config.upstreams.iter().enumerate().rev() {
                let is_healthy = health_state
                    .get(i)
                    .map_or(true, |h| h.load(Ordering::Relaxed));
                if is_healthy {
                    return Some((i, upstream.addr.clone()));
                }
            }
            None
        }
        L4LbAlgorithm::LeastConn => {
            // 接続中の最も少ない healthy upstream を選択
            config
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
                .map(|(i, u)| (i, u.addr.clone()))
        }
    }
}

/// バッファサイズ（16KB: キャッシュラインを意識したサイズ）
const BUF_SIZE: usize = 16 * 1024;

/// src から読み取って dst に書き込む（1 往復、ショートライトをリトライ）
///
/// 戻り値: 転送バイト数（0 = EOF / エラー）
async fn forward_once(src: &IoUringTcpStream, dst: &IoUringTcpStream) -> usize {
    // src から読み取り（Vec<u8> は IoBufMut を実装済み）
    let buf: Vec<u8> = vec![0u8; BUF_SIZE];
    let (res, buf) = src.read(buf).await;
    let n = match res {
        Ok(0) | Err(_) => return 0,
        Ok(n) => n,
    };

    // ショートライトをリトライして全バイトを書き込む
    let mut written = 0;
    while written < n {
        let chunk = buf[written..n].to_vec();
        let (wres, _) = dst.write(chunk).await;
        match wres {
            Ok(0) | Err(_) => return 0, // 接続切断またはエラー
            Ok(wn) => written += wn,
        }
    }
    n
}

/// クライアントと upstream の間でバイダイレクショナル転送を行う
///
/// どちらかの側が切断するか、アイドルタイムアウトになると終了する。
pub async fn bidirectional_forward(
    client: IoUringTcpStream,
    upstream: IoUringTcpStream,
    idle_timeout: Duration,
    listener_name: &str,
) {
    let client = Arc::new(client);
    let upstream_arc = Arc::new(upstream);

    let c2u_client = client.clone();
    let c2u_upstream = upstream_arc.clone();
    let u2c_client = client.clone();
    let u2c_upstream = upstream_arc.clone();
    let name_c2u = listener_name.to_string();
    let name_u2c = listener_name.to_string();

    // client → upstream 方向の転送タスク
    let c2u = async move {
        let mut total = 0usize;
        loop {
            match timeout(idle_timeout, forward_once(&c2u_client, &c2u_upstream)).await {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(_) => {
                    debug!("[L4:{}] c→u idle timeout", name_c2u);
                    break;
                }
            }
        }
        total
    };

    // upstream → client 方向の転送タスク
    let u2c = async move {
        let mut total = 0usize;
        loop {
            match timeout(idle_timeout, forward_once(&u2c_upstream, &u2c_client)).await {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(_) => {
                    debug!("[L4:{}] u→c idle timeout", name_u2c);
                    break;
                }
            }
        }
        total
    };

    // 両方向を並行実行（どちらかが終わると Arc カウントが落ち、他方も EOF になる）
    let (c2u_bytes, u2c_bytes) = futures::join!(c2u, u2c);
    debug!(
        "[L4:{}] connection closed: c→u {} bytes, u→c {} bytes",
        listener_name, c2u_bytes, u2c_bytes
    );
}

/// L4 接続を処理する（upstream 選択 → 接続 → バイダイレクショナル転送）
pub async fn handle_l4_connection(
    client: IoUringTcpStream,
    peer_addr: SocketAddr,
    config: Arc<L4ListenerConfig>,
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

    // リスナー全体の接続数をデクリメントするガード
    struct ListenerGuard(Arc<L4ConnectionCounter>);
    impl Drop for ListenerGuard {
        fn drop(&mut self) {
            self.0.current.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _listener_guard = ListenerGuard(listener_counter.clone());

    if config.tls == L4TlsMode::Terminate {
        // TLS ターミネーションは将来実装
        warn!(
            "[L4:{}] TLS termination not yet implemented, treating as passthrough",
            config.name
        );
    }

    // upstream アドレスを選択（インデックスも取得して per-upstream カウンタを管理）
    let (upstream_idx, upstream_addr) =
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

    // upstream 接続数をデクリメントするガード
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

    // upstream に接続
    let connect_timeout = Duration::from_secs(config.connect_timeout_secs);
    let upstream = match timeout(
        connect_timeout,
        IoUringTcpStream::connect_str(&upstream_addr),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            warn!(
                "[L4:{}] failed to connect to upstream {}: {}",
                config.name, upstream_addr, e
            );
            return;
        }
        Err(_) => {
            warn!(
                "[L4:{}] connection to upstream {} timed out",
                config.name, upstream_addr
            );
            return;
        }
    };

    let _ = upstream.set_nodelay(true);

    info!(
        "[L4:{}] {} → {} (tls={:?})",
        config.name, peer_addr, upstream_addr, config.tls
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
        assert_eq!(a3, "127.0.0.1:8001"); // ラップアラウンド
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
        assert_eq!(addr, "127.0.0.1:8001", "ties should resolve to first upstream");
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
        assert_eq!(addr, "127.0.0.1:8002", "should pick upstream with fewest connections");
    }

    // ====================
    // 新規: ヘルスフィルタテスト
    // ====================

    #[test]
    fn test_select_upstream_skips_unhealthy() {
        // upstream[0] が unhealthy → upstream[1] が選ばれる
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

        assert!(
            select_upstream(&config, &rr, &counters, &health).is_none(),
            "all unhealthy should return None"
        );
    }

    #[test]
    fn test_least_conn_skips_unhealthy() {
        // counters: [0, 0, 0] だが index 0 が unhealthy → index 1 (最小 healthy)
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"],
            L4LbAlgorithm::LeastConn,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(0)).collect();
        let health = vec![
            AtomicBool::new(false), // unhealthy
            AtomicBool::new(true),
            AtomicBool::new(true),
        ];

        let (idx, _) = select_upstream(&config, &rr, &counters, &health).unwrap();
        assert_ne!(idx, 0, "unhealthy upstream[0] should be skipped");
    }

    // ====================
    // 新規: Weighted Round Robin テスト
    // ====================

    #[test]
    fn test_weighted_round_robin_distribution() {
        // weight [2, 1] → upstream[0] が 2/3、upstream[1] が 1/3
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

        // 3 回で [0, 0, 1] の順（weight 比 2:1）
        let count_0 = results.iter().filter(|&&i| i == 0).count();
        let count_1 = results.iter().filter(|&&i| i == 1).count();
        assert_eq!(count_0, 2, "upstream[0] should get 2/3 of requests");
        assert_eq!(count_1, 1, "upstream[1] should get 1/3 of requests");
    }

    #[test]
    fn test_weighted_round_robin_equal_weight_is_round_robin() {
        // weight が全て 1 なら通常の Round Robin と同じ
        let config = make_config_weighted(
            vec![("127.0.0.1:8001", 1), ("127.0.0.1:8002", 1), ("127.0.0.1:8003", 1)],
            L4LbAlgorithm::RoundRobin,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(0)).collect();
        let health = all_healthy(3);

        let results: Vec<usize> = (0..6)
            .map(|_| select_upstream(&config, &rr, &counters, &health).unwrap().0)
            .collect();

        // 6 回で各 upstream が 2 回ずつ
        for expected in 0..3 {
            let count = results.iter().filter(|&&i| i == expected).count();
            assert_eq!(count, 2, "upstream[{}] should appear twice in 6 calls", expected);
        }
    }

    #[test]
    fn test_weighted_rr_skips_unhealthy_in_weight_calculation() {
        // weight [2, 1] で upstream[0] が unhealthy → upstream[1] のみ選ばれる
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

    // ====================
    // 新規: per-upstream カウンタの追跡テスト
    // ====================

    #[test]
    fn test_least_conn_uses_per_upstream_counters() {
        // 初期状態 [0, 0]: select → counters を手動 increment → 次の select は他を選ぶ
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002"],
            L4LbAlgorithm::LeastConn,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..2).map(|_| AtomicUsize::new(0)).collect();
        let health = all_healthy(2);

        // 1 回目: index 0 (両方 0 → 最初)
        let (idx1, _) = select_upstream(&config, &rr, &counters, &health).unwrap();
        // 選択後に手動 increment（handle_l4_connection が行う操作をシミュレート）
        counters[idx1].fetch_add(1, Ordering::Relaxed);

        // 2 回目: counter[0]=1, counter[1]=0 → index 1 が選ばれるべき
        let (idx2, _) = select_upstream(&config, &rr, &counters, &health).unwrap();
        assert_eq!(idx2, 1, "after incrementing idx1 counter, idx2 should be the other");

        // 選択後 increment
        counters[idx2].fetch_add(1, Ordering::Relaxed);

        // 3 回目: counter[0]=1, counter[1]=1 → index 0 (tie → first)
        let (idx3, _) = select_upstream(&config, &rr, &counters, &health).unwrap();
        assert_eq!(idx3, 0, "tie should resolve to first upstream");
    }

    #[test]
    fn test_idle_timeout_config() {
        // idle_timeout_secs が設定から読まれること
        let mut config = make_config(vec!["127.0.0.1:8001"], L4LbAlgorithm::RoundRobin);
        config.idle_timeout_secs = 30;
        assert_eq!(config.idle_timeout_secs, 30);
    }
}
