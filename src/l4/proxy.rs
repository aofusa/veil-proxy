//! L4 TCP プロキシ実装
//!
//! バイダイレクショナルストリーム転送、ロードバランシング、TLS パススルーを実装する。

use crate::config::{L4LbAlgorithm, L4ListenerConfig, L4TlsMode};
use crate::runtime::tcp::TcpStream as IoUringTcpStream;
use crate::runtime::time::timeout;
use ftlog::{debug, info, warn};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
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

    /// 次のインデックスを返す
    pub fn next(&self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        self.counter.fetch_add(1, Ordering::Relaxed) % len
    }
}

impl Default for RoundRobinState {
    fn default() -> Self {
        Self::new()
    }
}

/// upstream アドレスを選択する（ロードバランシング）
pub fn select_upstream(
    config: &L4ListenerConfig,
    rr_state: &RoundRobinState,
    conn_counters: &[AtomicUsize],
) -> Option<String> {
    if config.upstreams.is_empty() {
        return None;
    }

    match config.lb {
        L4LbAlgorithm::RoundRobin => {
            let idx = rr_state.next(config.upstreams.len());
            Some(config.upstreams[idx].addr.clone())
        }
        L4LbAlgorithm::LeastConn => {
            // 接続数が最小の upstream を選択
            let min_idx = conn_counters
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.load(Ordering::Relaxed))
                .map(|(i, _)| i)
                .unwrap_or(0);
            Some(config.upstreams[min_idx].addr.clone())
        }
    }
}

/// バッファサイズ（16KB: キャッシュラインを意識したサイズ）
const BUF_SIZE: usize = 16 * 1024;

/// src から読み取って dst に書き込む（1 往復）
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

    // 読み取ったデータだけを dst に書き込む
    let data = buf[..n].to_vec();
    let (wres, _) = dst.write(data).await;
    match wres {
        Ok(wn) => wn,
        Err(_) => 0,
    }
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

    // デクリメントガード（パニック時も安全）
    struct CounterGuard(Arc<L4ConnectionCounter>);
    impl Drop for CounterGuard {
        fn drop(&mut self) {
            self.0.current.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _guard = CounterGuard(listener_counter.clone());

    if config.tls == L4TlsMode::Terminate {
        // TLS ターミネーションは将来実装
        warn!(
            "[L4:{}] TLS termination not yet implemented, treating as passthrough",
            config.name
        );
    }

    // upstream アドレスを選択
    let upstream_addr = match select_upstream(&config, &rr_state, &conn_counters) {
        Some(addr) => addr,
        None => {
            warn!("[L4:{}] no upstream available", config.name);
            return;
        }
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

    // アイドルタイムアウト: デフォルト 10 分（L4 は長時間接続を想定）
    let idle_timeout = Duration::from_secs(600);
    bidirectional_forward(client, upstream, idle_timeout, &config.name).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{L4LbAlgorithm, L4ListenerConfig, L4TlsMode, L4UpstreamEntry};

    fn make_config(upstreams: Vec<&str>, lb: L4LbAlgorithm) -> L4ListenerConfig {
        L4ListenerConfig {
            name: "test".to_string(),
            listen: "0.0.0.0:9999".to_string(),
            upstreams: upstreams
                .into_iter()
                .map(|a| L4UpstreamEntry {
                    addr: a.to_string(),
                    weight: 1,
                })
                .collect(),
            lb,
            tls: L4TlsMode::None,
            max_connections: 0,
            health_check: None,
            connect_timeout_secs: 10,
        }
    }

    #[test]
    fn test_round_robin_selection() {
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"],
            L4LbAlgorithm::RoundRobin,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(0)).collect();

        // 連続して選択すると順番に回る
        let a0 = select_upstream(&config, &rr, &counters).unwrap();
        let a1 = select_upstream(&config, &rr, &counters).unwrap();
        let a2 = select_upstream(&config, &rr, &counters).unwrap();
        let a3 = select_upstream(&config, &rr, &counters).unwrap();

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

        // 接続数が少ない方（index 1）が選ばれる
        let addr = select_upstream(&config, &rr, &counters).unwrap();
        assert_eq!(addr, "127.0.0.1:8002");
    }

    #[test]
    fn test_select_upstream_empty() {
        let config = make_config(vec![], L4LbAlgorithm::RoundRobin);
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = vec![];

        assert!(select_upstream(&config, &rr, &counters).is_none());
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
        // 10 回呼んでも modulo で正しく動く
        for i in 0..10usize {
            assert_eq!(rr.next(3), i % 3);
        }
    }

    #[test]
    fn test_round_robin_single_upstream() {
        // upstream が 1 つの場合は常に同じアドレスが返る
        let config = make_config(vec!["127.0.0.1:8001"], L4LbAlgorithm::RoundRobin);
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = vec![AtomicUsize::new(0)];

        for _ in 0..5 {
            let addr = select_upstream(&config, &rr, &counters).unwrap();
            assert_eq!(addr, "127.0.0.1:8001");
        }
    }

    #[test]
    fn test_least_conn_single_upstream() {
        // LeastConn でも upstream が 1 つなら常にそれが返る
        let config = make_config(vec!["127.0.0.1:8001"], L4LbAlgorithm::LeastConn);
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = vec![AtomicUsize::new(10)];

        let addr = select_upstream(&config, &rr, &counters).unwrap();
        assert_eq!(addr, "127.0.0.1:8001");
    }

    #[test]
    fn test_least_conn_all_equal() {
        // 全 upstream の接続数が等しいとき index 0 を選ぶ
        let config = make_config(
            vec!["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"],
            L4LbAlgorithm::LeastConn,
        );
        let rr = RoundRobinState::new();
        let counters: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(3)).collect();

        let addr = select_upstream(&config, &rr, &counters).unwrap();
        assert_eq!(addr, "127.0.0.1:8001", "ties should resolve to first upstream");
    }

    #[test]
    fn test_max_connections_zero_is_unlimited() {
        // max_connections = 0 は無制限（制限チェックをスキップ）
        let config = make_config(vec!["127.0.0.1:8001"], L4LbAlgorithm::RoundRobin);
        assert_eq!(config.max_connections, 0);
        // 0 > 0 は false なので制限には引っかからない
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
        // max_connections > 0 のとき current >= max で拒否される
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
        // CounterGuard が drop されると current が減る
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
        assert_eq!(counter.current.load(Ordering::Relaxed), 0, "drop should decrement");
    }

    #[test]
    fn test_least_conn_picks_minimum() {
        // 接続数 [10, 1, 5] → index 1 (最小)
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

        let addr = select_upstream(&config, &rr, &counters).unwrap();
        assert_eq!(addr, "127.0.0.1:8002", "should pick upstream with fewest connections");
    }
}
