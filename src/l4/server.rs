//! L4 プロキシサーバー起動モジュール

use crate::config::{L4ListenerConfig, L4Protocol, L4TlsMode, SHUTDOWN_FLAG};
use crate::l4::health::{new_health_state, spawn_l4_health_checker};
use crate::l4::proxy::{
    handle_l4_connection, parse_upstream_targets, L4ConnectionCounter, RoundRobinState,
};
use crate::l4::udp::handle_l4_udp_listener;
use crate::runtime::tcp::TcpStream;
use crate::runtime::time::timeout;
use ftlog::{error, info, warn};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// L4（TCP/UDP）リスナー群を起動する。
///
/// # マルチワーカー化（F-156）について
///
/// `num_threads` は `entry.rs` のメインワーカー（TLS/H2C）が使うワーカースレッド数と
/// 同じ値を渡すこと。TCP リスナーは `num_threads` 個のワーカースレッドで
/// [`crate::server::create_listener`] を用いて `SO_REUSEPORT`（FreeBSD は
/// `SO_REUSEPORT_LB`）でリスナーソケットを複製し、各ワーカーが独立した reactor/uring
/// イベントループ上で accept する。これにより FreeBSD 実測で L4 が 1 コアに拘束されていた
/// ボトルネック（`docs/artifacts/freebsd_h2c_l4_perf_analysis.md` H1）を解消するとともに、
/// 全リスナー作成経路を `create_listener` に統一することで FreeBSD capsicum 有効時の
/// リスナー fd 権利制限（`limit_listener_rights`）が L4 だけ漏れていた抜け（同ドキュメント
/// 背景節参照）も同時に塞ぐ。
///
/// ロードバランシング状態（`rr_state`/`conn_counters`/`listener_counter`）とヘルスチェッカー
/// はリスナーにつき 1 回だけスレッド生成ループの**外**で作成し、`Arc` で全ワーカーへ配る。
/// ワーカーごとに独立させると（1）ラウンドロビンが偏る、（2）LeastConn の最小接続選択が
/// 壊れる、（3）`max_connections` の上限がワーカー数倍に緩む、という 3 つの不具合が生じる。
///
/// UDP リスナーは本関数の対象外で、従来どおりシングルスレッドのまま変更しない。UDP の
/// reuseport 分散はセッション管理（`sessions`）がワーカーごとに分断される問題があり、
/// 別途設計（セッション共有かコンシステントハッシュ等）が必要なため、本チケットのスコープ
/// では見送る。
pub fn spawn_l4_listeners(
    listeners: &[L4ListenerConfig],
    num_threads: usize,
    balancing: crate::config::ReuseportBalancing,
) {
    // CPU コアピン留め用のコア ID 一覧。`entry.rs` の H2C ワーカーと同じ
    // `core_affinity::get_core_ids()` + `thread_id % ids.len()` 方式を、`entry.rs` を
    // 変更せずに済むようここで自前に取得する。
    let core_ids = core_affinity::get_core_ids();
    let core_ids_available = core_ids.as_ref().map(|ids| ids.len()).unwrap_or(0);

    for config in listeners {
        if config.upstreams.is_empty() {
            warn!("[L4:{}] no upstreams configured, skipping", config.name);
            continue;
        }

        // 起動時に解決できる上流はキャッシュ、未解決ホスト名は接続時に解決（B-33）
        let upstream_targets = Arc::new(parse_upstream_targets(config));

        // F-124: UDP は DTLS 非対象。TLS 終端/パススルーを要求する UDP 構成は
        // 起動時に警告して none 扱いへ強制する（エラーで起動を止めるとオペレータの
        // 意図しない設定ミス 1 件で他リスナーまで巻き込むため、安全側の none 化 + 警告に留める）。
        let mut config = config.clone();
        if config.protocol == L4Protocol::Udp && config.tls != L4TlsMode::None {
            warn!(
                "[L4:{}] protocol=udp does not support TLS (DTLS unsupported); tls={:?} is ignored and treated as none",
                config.name, config.tls
            );
            config.tls = L4TlsMode::None;
        }
        let config = Arc::new(config);
        let n_upstreams = config.upstreams.len();

        // F-156: ロードバランシング状態はリスナーにつき 1 個だけ生成し、全ワーカーへ
        // Arc で共有する（ワーカーごとに独立させてはならない。上のドキュメントコメント参照）。
        let rr_state = Arc::new(RoundRobinState::new());
        let conn_counters: Arc<Vec<AtomicUsize>> =
            Arc::new((0..n_upstreams).map(|_| AtomicUsize::new(0)).collect());
        let listener_counter = Arc::new(L4ConnectionCounter::new());

        // ヘルスチェッカーもリスナーにつき 1 回だけ起動する（ワーカーごとに起動すると
        // 上流へのヘルスチェック要求がワーカー数倍になる）。
        let health_state = new_health_state(n_upstreams);
        spawn_l4_health_checker(config.clone(), health_state.clone());

        match config.protocol {
            L4Protocol::Udp => {
                // UDP は現状維持でシングルスレッドのまま（上のドキュメントコメント参照）。
                info!(
                    "[L4:{}] starting {:?} listener on {} ({} upstreams, lb={:?}, idle_timeout={}s)",
                    config.name,
                    config.protocol,
                    config.listen,
                    n_upstreams,
                    config.lb,
                    config.idle_timeout_secs
                );

                let config = config.clone();
                let upstream_targets = upstream_targets.clone();
                let rr_state = rr_state.clone();
                let conn_counters = conn_counters.clone();
                let listener_counter = listener_counter.clone();
                let health_state = health_state.clone();

                thread::spawn(move || {
                    let listen_addr: SocketAddr = match config.listen.parse() {
                        Ok(addr) => addr,
                        Err(e) => {
                            error!(
                                "[L4:{}] invalid listen address '{}': {}",
                                config.name, config.listen, e
                            );
                            return;
                        }
                    };

                    crate::runtime::block_on(async move {
                        handle_l4_udp_listener(
                            listen_addr,
                            config,
                            upstream_targets,
                            rr_state,
                            conn_counters,
                            listener_counter,
                            health_state,
                        )
                        .await;
                    });
                });
            }
            L4Protocol::Tcp => {
                let listen_addr: SocketAddr = match config.listen.parse() {
                    Ok(addr) => addr,
                    Err(e) => {
                        error!(
                            "[L4:{}] invalid listen address '{}': {}",
                            config.name, config.listen, e
                        );
                        continue;
                    }
                };

                info!(
                    "[L4:{}] starting {:?} listener on {} ({} upstreams, lb={:?}, idle_timeout={}s, {} workers)",
                    config.name,
                    config.protocol,
                    config.listen,
                    n_upstreams,
                    config.lb,
                    config.idle_timeout_secs,
                    num_threads
                );

                if core_ids_available > 0 {
                    info!(
                        "[L4:{}] CPU Affinity: {} cores available, pinning {} worker threads",
                        config.name, core_ids_available, num_threads
                    );
                } else {
                    warn!(
                        "[L4:{}] CPU Affinity: could not detect core IDs, workers will not be pinned",
                        config.name
                    );
                }

                for thread_id in 0..num_threads {
                    let config = config.clone();
                    let upstream_targets = upstream_targets.clone();
                    let rr_state = rr_state.clone();
                    let conn_counters = conn_counters.clone();
                    let listener_counter = listener_counter.clone();
                    let health_state = health_state.clone();
                    // `ReuseportBalancing` は `Copy` なので、move クロージャは値コピーを
                    // 取り込む（明示的な再束縛は不要）。
                    let assigned_core = core_ids.as_ref().map(|ids| ids[thread_id % ids.len()]);

                    // spawn 失敗時のログ用にリスナー名だけ先に控える（`config` は
                    // スレッドクロージャへ move されるため、spawn 後には参照できない）。
                    // 起動時 1 回だけのコールドパスなので clone のコストは問題にならない。
                    let listener_name_for_err = config.name.clone();

                    // I/O ワーカースレッドと同じ 8MB スタックで起動する（`entry.rs` の
                    // `spawn_worker_thread` と同じ理由: 深いネストの接続ハンドラ future が
                    // 既定 2MB スタックを溢れさせ得るため）。`entry.rs` のヘルパーは
                    // private なので、ここでは同じ設定を自前で組み立てる。
                    let build_result =
                        thread::Builder::new()
                            .stack_size(8 * 1024 * 1024)
                            .spawn(move || {
                                if let Some(core_id) = assigned_core {
                                    if core_affinity::set_for_current(core_id) {
                                        info!(
                                            "[L4:{} Worker {}] Pinned to CPU core {:?}",
                                            config.name, thread_id, core_id
                                        );
                                    } else {
                                        warn!(
                                            "[L4:{} Worker {}] Failed to pin to CPU core {:?}",
                                            config.name, thread_id, core_id
                                        );
                                    }
                                }

                                crate::runtime::block_on(async move {
                                    // F-156: 全リスナー作成経路を `create_listener` に統一する
                                    // ことで SO_REUSEPORT(_LB) 分散と FreeBSD capsicum の
                                    // リスナー fd 権利制限を L4 にも適用する。
                                    let listener = match crate::server::create_listener(
                                        listen_addr,
                                        balancing,
                                        num_threads,
                                        thread_id,
                                    ) {
                                        Ok(l) => l,
                                        Err(e) => {
                                            error!(
                                                "[L4:{} Worker {}] bind error on {}: {}",
                                                config.name, thread_id, listen_addr, e
                                            );
                                            return;
                                        }
                                    };

                                    info!(
                                        "[L4:{} Worker {}] listening on {}",
                                        config.name, thread_id, listen_addr
                                    );

                                    // F-46: L4 接続ハンドラの型付きタスクプール
                                    let conn_pool = crate::runtime::TaskPool::new();

                                    loop {
                                        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                                            info!(
                                                "[L4:{} Worker {}] shutting down",
                                                config.name, thread_id
                                            );
                                            break;
                                        }

                                        let accept_result =
                                            timeout(Duration::from_secs(1), listener.accept())
                                                .await;

                                        let (stream, peer_addr) = match accept_result {
                                            Ok(Ok(s)) => s,
                                            Ok(Err(e)) => {
                                                error!(
                                                    "[L4:{} Worker {}] accept error: {}",
                                                    config.name, thread_id, e
                                                );
                                                continue;
                                            }
                                            Err(_) => continue,
                                        };

                                        // 同時接続数の上限判定は `handle_l4_connection`
                                        // （`src/l4/proxy.rs`）内で `listener_counter` を用いて
                                        // 行われ、accept ループ側では行わない。`listener_counter`
                                        // は全ワーカーで共有される `Arc` のため、TLS/H2C ワーカーの
                                        // ような `batch_accepted` ローカルカウンタは不要。
                                        let handle_accepted =
                                            |stream: TcpStream, peer_addr: SocketAddr| {
                                                let _ = stream.set_nodelay(true);

                                                let config_c = config.clone();
                                                let targets_c = upstream_targets.clone();
                                                let rr_c = rr_state.clone();
                                                let counters_c = conn_counters.clone();
                                                let listener_counter_c = listener_counter.clone();
                                                let health_c = health_state.clone();

                                                crate::system::spawn_pooled_with_panic_catch(
                                                    &conn_pool,
                                                    async move {
                                                        handle_l4_connection(
                                                            stream,
                                                            peer_addr,
                                                            config_c,
                                                            targets_c,
                                                            rr_c,
                                                            counters_c,
                                                            listener_counter_c,
                                                            health_c,
                                                        )
                                                        .await;
                                                    },
                                                );
                                            };

                                        handle_accepted(stream, peer_addr);

                                        // F-155 と同じ Burst Accept パターン。io_uring バックエンド
                                        // （`veil_rt_uring`）の `TcpListener` には `accept_batch` が
                                        // 存在しない（io_uring パスのロジックは変更しない方針）ため、
                                        // reactor バックエンド（`veil_rt_reactor`）限定で有効にする。
                                        // クロージャは捕捉状態を書き換えないため `Fn` として
                                        // 推論される（TLS/H2C ワーカーの `batch_accepted`
                                        // に相当する可変状態を持たない）。`&mut` を取ると
                                        // io_uring ビルドで不要な `mut` 束縛になるため、
                                        // ここでは値渡しする（この周回での最後の使用）。
                                        #[cfg(veil_rt_reactor)]
                                        {
                                            if let Err(e) =
                                                listener.accept_batch(31, handle_accepted)
                                            {
                                                error!(
                                                    "[L4:{} Worker {}] accept error: {}",
                                                    config.name, thread_id, e
                                                );
                                            }
                                        }
                                    }

                                    info!("[L4:{} Worker {}] stopped", config.name, thread_id);
                                });
                            });

                    if let Err(e) = build_result {
                        error!(
                            "[L4:{}] failed to spawn worker thread {}: {}",
                            listener_name_for_err, thread_id, e
                        );
                    }
                }
            }
        }
    }
}
