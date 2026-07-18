//! L4 UDP プロキシ実装（F-124）
//!
//! nginx stream の UDP / Envoy UDP proxy 相当のセッションテーブル方式で、
//! コネクションレスな UDP をクライアントアドレスをキーにした疑似セッションとして扱う。
//!
//! ## セッション管理設計
//!
//! - リスナー UDP ソケット 1 本（`Rc<UdpSocket>`）を全セッションで共有する。
//!   thread-per-core（1 スレッドに 1 io_uring リング／reactor poller）のため、
//!   `Rc`/`RefCell` によるロックレス共有で十分（AGENTS.md: 不要なロックを増やさない）。
//! - セッションテーブルは `HashMap<SocketAddr, Rc<UdpSession>>`。キーはクライアントの
//!   送信元アドレス。
//! - 新規クライアントパケット到着時、ラウンドロビン/最小接続数で upstream を選択し、
//!   その upstream へ `connect(2)` した専用 UDP ソケットを作成してセッションに束ねる
//!   （`connect` 済みソケットは `send`/`recv` で宛先指定を省略できる）。
//! - クライアント → upstream 方向はリスナーの recvfrom ループが直接
//!   `session.upstream.send()` する（追加タスク不要）。
//! - upstream → クライアント方向は、セッションごとに 1 つ spawn する専用タスクが
//!   upstream ソケットを `recv` ループし、共有リスナーソケット経由で
//!   `send_to(client_addr)` して返送する。
//! - アイドルタイムアウト: セッションは最終アクティビティ時刻（`Cell<Instant>`）を持ち、
//!   クライアント→upstream・upstream→クライアントの双方向の送受信で更新する。
//!   per-session タスクは `runtime::time::timeout` で短い間隔（1 秒、ただし
//!   `idle_timeout_secs` がそれより短ければそちらに合わせる）で upstream の受信を
//!   待ち、タイムアウトのたびに最終アクティビティからの経過を確認して
//!   `idle_timeout_secs` を超えていればセッションを退去する。
//! - `max_connections`（0 = 無制限）は同時セッション数の上限として扱う。
//!
//! ## ヘルスチェックの扱い
//!
//! UDP はコネクションレスのため疎通確認が本質的に困難（応答の有無だけでは判定不能な
//! プロトコルが大半）。UDP バックエンドのヘルスチェックも既存の TCP connect ベースの
//! ままとし（`health.rs` は protocol を見ない）、UDP 到達性そのものの確認は scope 外とする。
//!
//! ## ホットパス絶対規則との整合
//!
//! パケット（データグラム）ごとの新規ヒープ確保を避けるため、受信バッファは
//! リスナーループ・per-session タスクそれぞれで **一度だけ** 確保し、ループ内で
//! 使い回す（L4 TCP の `forward_direction` と同じ方針）。セッション作成（`HashMap`
//! エントリ・`UdpSession`・upstream ソケットの確保）はコネクションレスの性質上
//! 避けられないコールドパス側の処理であり許容する。

use crate::config::{L4ListenerConfig, SHUTDOWN_FLAG};
use crate::l4::health::L4HealthState;
use crate::l4::proxy::{
    resolve_upstream_target, select_upstream, L4ConnectionCounter, L4UpstreamTarget,
    RoundRobinState,
};
use crate::runtime::time::timeout;
use crate::runtime::udp::UdpSocket;
use crate::runtime::TaskPool;
use ftlog::{debug, error, info, warn};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// UDP データグラムの受信バッファサイズ。IPv4/IPv6 いずれの経路でも収まる
/// 実用上の UDP ペイロード上限（65507 バイト）を切り上げた値。
const UDP_BUF_SIZE: usize = 65536;

/// セッションテーブルの型（クライアントアドレス → セッション）。
type SessionTable = Rc<RefCell<HashMap<SocketAddr, Rc<UdpSession>>>>;

/// 1 クライアントぶんの UDP 疑似セッション。
struct UdpSession {
    /// upstream へ `connect` 済みの専用ソケット。
    upstream: UdpSocket,
    /// 最終アクティビティ時刻（双方向の送受信で更新）。
    last_active: Cell<Instant>,
    /// 選択された upstream のインデックス（接続数カウンタ減算用）。
    upstream_idx: usize,
}

/// L4 UDP リスナーを処理する（server.rs から呼ぶ）。
///
/// リスナー UDP ソケットを bind し、`recvfrom` ループでクライアントパケットを
/// セッションテーブルへ振り分ける。TCP の accept ループと異なり、コネクションレスの
/// ため単一の UDP ソケットに対する受信ループのみで完結する。
#[allow(clippy::too_many_arguments)]
pub async fn handle_l4_udp_listener(
    listen_addr: SocketAddr,
    config: Arc<L4ListenerConfig>,
    upstream_targets: Arc<Vec<L4UpstreamTarget>>,
    rr_state: Arc<RoundRobinState>,
    conn_counters: Arc<Vec<std::sync::atomic::AtomicUsize>>,
    listener_counter: Arc<L4ConnectionCounter>,
    health_state: L4HealthState,
) {
    let listener = match UdpSocket::bind(listen_addr) {
        Ok(s) => Rc::new(s),
        Err(e) => {
            error!(
                "[L4:{}] UDP bind error on {}: {}",
                config.name, listen_addr, e
            );
            return;
        }
    };

    info!("[L4:{}] UDP listening on {}", config.name, listen_addr);

    let sessions: SessionTable = Rc::new(RefCell::new(HashMap::new()));
    // per-session（upstream → client 方向）タスク用のプール（F-46 型付きタスクプール）。
    let session_pool = TaskPool::new();

    // リスナー受信バッファ。ホットパス絶対規則によりループ内で再確保しない
    // （L4 TCP の forward_direction と同様、コネクション/リスナー確立時に一度だけ確保）。
    #[allow(clippy::uninit_vec)]
    let mut buf: Vec<u8> = {
        let mut b = Vec::with_capacity(UDP_BUF_SIZE);
        // SAFETY: recv_from がカーネルから受信した先頭 n バイトを書き込み、直後に
        // valid スライス `&buf[..n]` のみを参照する。[n..UDP_BUF_SIZE] を読むことはない。
        unsafe { b.set_len(UDP_BUF_SIZE) };
        b
    };

    loop {
        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            info!("[L4:{}] UDP listener shutting down", config.name);
            break;
        }

        let (n, client_addr) =
            match timeout(Duration::from_secs(1), listener.recv_from(&mut buf)).await {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    warn!("[L4:{}] UDP recvfrom error: {}", config.name, e);
                    continue;
                }
                Err(_) => continue, // 1 秒ごとに SHUTDOWN_FLAG を再チェックするための無活動タイムアウト
            };

        let session = match sessions.borrow().get(&client_addr) {
            Some(s) => {
                s.last_active.set(Instant::now());
                Some(s.clone())
            }
            None => None,
        };

        let session = match session {
            Some(s) => s,
            None => {
                // 新規クライアント: 接続数上限チェック（max_connections = 同時セッション数上限）。
                if config.max_connections > 0 && sessions.borrow().len() >= config.max_connections {
                    warn!(
                        "[L4:{}] UDP session limit ({}) reached, dropping packet from {}",
                        config.name, config.max_connections, client_addr
                    );
                    continue;
                }

                let (upstream_idx, upstream_addr_str) =
                    match select_upstream(&config, &rr_state, &conn_counters, &health_state) {
                        Some(pair) => pair,
                        None => {
                            warn!("[L4:{}] no healthy upstream available", config.name);
                            continue;
                        }
                    };

                let socket_addr = match upstream_targets.get(upstream_idx) {
                    Some(target) => match resolve_upstream_target(target).await {
                        Some(a) => a,
                        None => {
                            warn!(
                                "[L4:{}] failed to resolve upstream {}",
                                config.name, upstream_addr_str
                            );
                            continue;
                        }
                    },
                    None => {
                        warn!(
                            "[L4:{}] upstream_targets index {} out of range",
                            config.name, upstream_idx
                        );
                        continue;
                    }
                };

                // upstream 用のソケットは未使用ローカルポートへ bind してから connect する。
                let bind_addr: SocketAddr = if socket_addr.is_ipv6() {
                    "[::]:0".parse().unwrap()
                } else {
                    "0.0.0.0:0".parse().unwrap()
                };
                let upstream_socket = match UdpSocket::bind(bind_addr) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            "[L4:{}] failed to create upstream UDP socket: {}",
                            config.name, e
                        );
                        continue;
                    }
                };
                if let Err(e) = upstream_socket.connect(socket_addr) {
                    warn!(
                        "[L4:{}] failed to connect upstream UDP socket to {}: {}",
                        config.name, upstream_addr_str, e
                    );
                    continue;
                }

                if let Some(c) = conn_counters.get(upstream_idx) {
                    c.fetch_add(1, Ordering::Relaxed);
                }
                listener_counter.current.fetch_add(1, Ordering::Relaxed);
                listener_counter.total.fetch_add(1, Ordering::Relaxed);

                let session = Rc::new(UdpSession {
                    upstream: upstream_socket,
                    last_active: Cell::new(Instant::now()),
                    upstream_idx,
                });
                sessions.borrow_mut().insert(client_addr, session.clone());

                debug!(
                    "[L4:{}] new UDP session {} → {}",
                    config.name, client_addr, upstream_addr_str
                );

                // upstream → client 方向を担当する per-session タスクを起動する。
                let listener_clone = listener.clone();
                let sessions_clone = sessions.clone();
                let session_clone = session.clone();
                let config_clone = config.clone();
                let conn_counters_clone = conn_counters.clone();
                let listener_counter_clone = listener_counter.clone();

                crate::system::spawn_pooled_with_panic_catch(&session_pool, async move {
                    run_udp_session_upstream_to_client(
                        listener_clone,
                        session_clone,
                        client_addr,
                        sessions_clone,
                        config_clone,
                        conn_counters_clone,
                        listener_counter_clone,
                    )
                    .await;
                });

                session
            }
        };

        if let Err(e) = session.upstream.send(&buf[..n]).await {
            debug!(
                "[L4:{}] UDP send to upstream failed for session {}: {}",
                config.name, client_addr, e
            );
            // セッションを破棄する（per-session タスクは次の recv タイムアウトで自然に終了する）。
            sessions.borrow_mut().remove(&client_addr);
        }
    }

    info!("[L4:{}] UDP listener stopped", config.name);
}

/// upstream → client 方向の転送を担当する per-session タスク。
///
/// upstream ソケットを `recv` ループし、受信した応答をリスナーソケット経由で
/// `send_to(client_addr)` してクライアントへ返す。`idle_timeout_secs` 無通信で
/// セッションを退去する。
#[allow(clippy::too_many_arguments)]
async fn run_udp_session_upstream_to_client(
    listener: Rc<UdpSocket>,
    session: Rc<UdpSession>,
    client_addr: SocketAddr,
    sessions: SessionTable,
    config: Arc<L4ListenerConfig>,
    conn_counters: Arc<Vec<std::sync::atomic::AtomicUsize>>,
    listener_counter: Arc<L4ConnectionCounter>,
) {
    let idle_timeout = Duration::from_secs(config.idle_timeout_secs.max(1));
    // 1 秒間隔（ただし idle_timeout がそれより短い設定であれば idle_timeout に合わせる）で
    // upstream 受信を再試行し、都度アイドル経過を確認する。
    let poll_interval = idle_timeout.min(Duration::from_secs(1));

    // per-session バッファ。セッション生存中に一度だけ確保しループ内で再利用する
    // （ホットパス絶対規則: パケットごとの新規確保を禁止）。
    #[allow(clippy::uninit_vec)]
    let mut buf: Vec<u8> = {
        let mut b = Vec::with_capacity(UDP_BUF_SIZE);
        // SAFETY: recv がカーネルから受信した先頭 n バイトを書き込み、直後に
        // valid スライス `&buf[..n]` のみを参照する。
        unsafe { b.set_len(UDP_BUF_SIZE) };
        b
    };

    loop {
        match timeout(poll_interval, session.upstream.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                session.last_active.set(Instant::now());
                if let Err(e) = listener.send_to(&buf[..n], client_addr).await {
                    debug!(
                        "[L4:{}] UDP send_to client {} failed: {}",
                        config.name, client_addr, e
                    );
                    break;
                }
            }
            Ok(Err(e)) => {
                debug!(
                    "[L4:{}] UDP upstream recv error for session {}: {}",
                    config.name, client_addr, e
                );
                break;
            }
            Err(_) => {
                // poll_interval 経過。実際のアイドル時間を last_active と比較する。
                if session.last_active.get().elapsed() >= idle_timeout {
                    debug!(
                        "[L4:{}] UDP session {} idle timeout",
                        config.name, client_addr
                    );
                    break;
                }
            }
        }
    }

    // セッション後始末: テーブルから除去し、接続数カウンタを戻す。
    sessions.borrow_mut().remove(&client_addr);
    if let Some(c) = conn_counters.get(session.upstream_idx) {
        c.fetch_sub(1, Ordering::Relaxed);
    }
    listener_counter.current.fetch_sub(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{L4LbAlgorithm, L4Protocol, L4TlsMode, L4UpstreamEntry};
    use std::sync::atomic::AtomicUsize;

    fn make_udp_config(upstreams: Vec<&str>, max_connections: usize) -> L4ListenerConfig {
        L4ListenerConfig {
            name: "test-udp".to_string(),
            listen: "0.0.0.0:0".to_string(),
            upstreams: upstreams
                .into_iter()
                .map(|a| L4UpstreamEntry {
                    addr: a.to_string(),
                    weight: 1,
                })
                .collect(),
            protocol: L4Protocol::Udp,
            lb: L4LbAlgorithm::RoundRobin,
            tls: L4TlsMode::None,
            max_connections,
            health_check: None,
            connect_timeout_secs: 10,
            idle_timeout_secs: 1,
        }
    }

    #[test]
    fn test_session_table_insert_and_lookup() {
        let sessions: SessionTable = Rc::new(RefCell::new(HashMap::new()));
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();

        assert!(sessions.borrow().get(&addr).is_none());

        // ダミーの upstream ソケットで疑似セッションを作る（bind のみ、connect はしない）。
        let upstream = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).expect("bind upstream");
        let session = Rc::new(UdpSession {
            upstream,
            last_active: Cell::new(Instant::now()),
            upstream_idx: 0,
        });
        sessions.borrow_mut().insert(addr, session);

        assert!(sessions.borrow().get(&addr).is_some());
        assert_eq!(sessions.borrow().len(), 1);
    }

    #[test]
    fn test_session_idle_timeout_detection() {
        let upstream = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).expect("bind upstream");
        let session = UdpSession {
            upstream,
            last_active: Cell::new(Instant::now() - Duration::from_secs(10)),
            upstream_idx: 0,
        };
        let idle_timeout = Duration::from_secs(1);
        assert!(
            session.last_active.get().elapsed() >= idle_timeout,
            "session inactive for 10s must be considered idle when idle_timeout=1s"
        );
    }

    #[test]
    fn test_session_activity_resets_idle_clock() {
        let upstream = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).expect("bind upstream");
        let session = UdpSession {
            upstream,
            last_active: Cell::new(Instant::now() - Duration::from_secs(10)),
            upstream_idx: 0,
        };
        let idle_timeout = Duration::from_secs(5);
        assert!(session.last_active.get().elapsed() >= idle_timeout);

        // アクティビティで last_active を更新すると即座に idle 判定から外れる。
        session.last_active.set(Instant::now());
        assert!(session.last_active.get().elapsed() < idle_timeout);
    }

    #[test]
    fn test_max_connections_zero_is_unlimited_for_udp() {
        let config = make_udp_config(vec!["127.0.0.1:9999"], 0);
        assert_eq!(config.max_connections, 0);
        let sessions: SessionTable = Rc::new(RefCell::new(HashMap::new()));
        // 0 は「無制限」なので、セッション数に関わらず上限チェックは常に false。
        let should_reject =
            config.max_connections > 0 && sessions.borrow().len() >= config.max_connections;
        assert!(!should_reject);
    }

    #[test]
    fn test_max_connections_limit_for_udp_sessions() {
        let config = make_udp_config(vec!["127.0.0.1:9999"], 1);
        let sessions: SessionTable = Rc::new(RefCell::new(HashMap::new()));
        let upstream = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).expect("bind upstream");
        let session = Rc::new(UdpSession {
            upstream,
            last_active: Cell::new(Instant::now()),
            upstream_idx: 0,
        });
        sessions
            .borrow_mut()
            .insert("127.0.0.1:1".parse().unwrap(), session);

        let should_reject =
            config.max_connections > 0 && sessions.borrow().len() >= config.max_connections;
        assert!(
            should_reject,
            "session count == max_connections should reject new session"
        );
    }

    #[test]
    fn test_conn_counters_len_matches_upstreams() {
        let config = make_udp_config(vec!["127.0.0.1:1", "127.0.0.1:2"], 0);
        let counters: Vec<AtomicUsize> = (0..config.upstreams.len())
            .map(|_| AtomicUsize::new(0))
            .collect();
        assert_eq!(counters.len(), 2);
    }
}
