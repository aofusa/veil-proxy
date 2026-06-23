//! L4 プロキシサーバー起動モジュール

use crate::config::{L4ListenerConfig, SHUTDOWN_FLAG};
use crate::l4::health::{new_health_state, spawn_l4_health_checker};
use crate::l4::proxy::{
    handle_l4_connection, parse_upstream_addrs, L4ConnectionCounter, RoundRobinState,
};
use crate::runtime::tcp::TcpListener;
use crate::runtime::time::timeout;
use crate::system::spawn_with_panic_catch;
use ftlog::{error, info, warn};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub fn spawn_l4_listeners(listeners: &[L4ListenerConfig]) {
    for config in listeners {
        if config.upstreams.is_empty() {
            warn!("[L4:{}] no upstreams configured, skipping", config.name);
            continue;
        }

        // upstream アドレスを起動時に一度だけパース（hot path での DNS 解決を排除）
        let parsed_addrs = match parse_upstream_addrs(config) {
            Ok(addrs) => Arc::new(addrs),
            Err(e) => {
                error!("[L4:{}] failed to parse upstream addresses: {}", config.name, e);
                continue;
            }
        };

        let config = Arc::new(config.clone());
        let n_upstreams = config.upstreams.len();

        let health_state = new_health_state(n_upstreams);
        spawn_l4_health_checker(config.clone(), health_state.clone());

        info!(
            "[L4:{}] starting listener on {} ({} upstreams, lb={:?}, idle_timeout={}s)",
            config.name, config.listen, n_upstreams, config.lb, config.idle_timeout_secs
        );

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

            let rr_state = Arc::new(RoundRobinState::new());
            let conn_counters: Arc<Vec<AtomicUsize>> =
                Arc::new((0..n_upstreams).map(|_| AtomicUsize::new(0)).collect());
            let listener_counter = Arc::new(L4ConnectionCounter::new());

            crate::runtime::block_on(async move {
                let listener = match TcpListener::bind(listen_addr) {
                    Ok(l) => l,
                    Err(e) => {
                        error!(
                            "[L4:{}] bind error on {}: {}",
                            config.name, listen_addr, e
                        );
                        return;
                    }
                };

                info!("[L4:{}] listening on {}", config.name, listen_addr);

                loop {
                    if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                        info!("[L4:{}] shutting down", config.name);
                        break;
                    }

                    let accept_result =
                        timeout(Duration::from_secs(1), listener.accept()).await;

                    let (stream, peer_addr) = match accept_result {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            error!("[L4:{}] accept error: {}", config.name, e);
                            continue;
                        }
                        Err(_) => continue,
                    };

                    let _ = stream.set_nodelay(true);

                    let config_clone = config.clone();
                    let parsed_addrs_clone = parsed_addrs.clone();
                    let rr_clone = rr_state.clone();
                    let counters_clone = conn_counters.clone();
                    let listener_counter_clone = listener_counter.clone();
                    let health_clone = health_state.clone();

                    spawn_with_panic_catch(async move {
                        handle_l4_connection(
                            stream,
                            peer_addr,
                            config_clone,
                            parsed_addrs_clone,
                            rr_clone,
                            counters_clone,
                            listener_counter_clone,
                            health_clone,
                        )
                        .await;
                    });
                }

                info!("[L4:{}] stopped", config.name);
            });
        });
    }
}
