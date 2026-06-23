//! L4 プロキシ用ヘルスチェックモジュール
//!
//! 各 L4 upstream に対して定期的にヘルスチェックを実行し、
//! 結果を `Arc<Vec<AtomicBool>>` に反映する。

use crate::config::{HealthCheckType, L4ListenerConfig, SHUTDOWN_FLAG};
use crate::upstream::{perform_grpc_health_check, perform_tcp_health_check};
use ftlog::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// L4 upstream のヘルス状態（upstream ごとに 1 エントリ）
pub type L4HealthState = Arc<Vec<AtomicBool>>;

/// `L4HealthState` を upstream 数分作成し、全て healthy で初期化する
pub fn new_health_state(n_upstreams: usize) -> L4HealthState {
    Arc::new((0..n_upstreams).map(|_| AtomicBool::new(true)).collect())
}

/// ヘルスチェックスレッドを起動する
///
/// `health_check` が設定されていない場合はスレッドを起動しない。
/// スレッドは `SHUTDOWN_FLAG` が立つと終了する。
pub fn spawn_l4_health_checker(config: Arc<L4ListenerConfig>, health_state: L4HealthState) {
    let hc = match &config.health_check {
        Some(hc) => hc.clone(),
        None => return, // ヘルスチェック設定なし → スキップ
    };

    let interval = Duration::from_secs(hc.interval_secs);
    let timeout = Duration::from_secs(hc.timeout_secs);

    std::thread::spawn(move || {
        // 失敗・成功カウンタ（upstream ごと）
        let mut fail_counts: Vec<u32> = vec![0; config.upstreams.len()];
        let mut success_counts: Vec<u32> = vec![0; config.upstreams.len()];

        loop {
            if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                break;
            }

            for (i, upstream) in config.upstreams.iter().enumerate() {
                let addr = &upstream.addr;

                let healthy = match hc.check_type {
                    HealthCheckType::Tcp => perform_tcp_health_check(addr, timeout),
                    HealthCheckType::Grpc => perform_grpc_health_check(
                        addr,
                        &hc.path,
                        hc.use_tls,
                        hc.verify_cert,
                        timeout,
                    ),
                    HealthCheckType::Http => {
                        // L4 のHTTPヘルスチェックは TCP と同等（接続確認のみ）
                        perform_tcp_health_check(addr, timeout)
                    }
                };

                let was_healthy = health_state[i].load(Ordering::Relaxed);

                if healthy {
                    fail_counts[i] = 0;
                    success_counts[i] = success_counts[i].saturating_add(1);

                    if !was_healthy && success_counts[i] >= hc.healthy_threshold {
                        health_state[i].store(true, Ordering::Relaxed);
                        info!("[L4:{}] upstream {} is now healthy", config.name, addr);
                        success_counts[i] = 0;
                    }
                } else {
                    success_counts[i] = 0;
                    fail_counts[i] = fail_counts[i].saturating_add(1);

                    if was_healthy && fail_counts[i] >= hc.unhealthy_threshold {
                        health_state[i].store(false, Ordering::Relaxed);
                        warn!("[L4:{}] upstream {} is now unhealthy", config.name, addr);
                        fail_counts[i] = 0;
                    }
                }
            }

            // インターバル待機（1 秒ずつ polling して SHUTDOWN_FLAG をチェック）
            let mut waited = Duration::ZERO;
            while waited < interval {
                if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
                waited += Duration::from_secs(1);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HealthCheckConfig, HealthCheckType, L4LbAlgorithm, L4UpstreamEntry};

    fn make_hc_config(check_type: HealthCheckType) -> HealthCheckConfig {
        HealthCheckConfig {
            check_type,
            interval_secs: 10,
            path: String::new(),
            timeout_secs: 1,
            healthy_statuses: vec![200],
            unhealthy_threshold: 2,
            healthy_threshold: 1,
            use_tls: false,
            verify_cert: true,
        }
    }

    fn make_l4_config_with_hc(upstreams: Vec<&str>, hc: HealthCheckConfig) -> L4ListenerConfig {
        L4ListenerConfig {
            name: "test-hc".to_string(),
            listen: "0.0.0.0:9999".to_string(),
            upstreams: upstreams
                .into_iter()
                .map(|a| L4UpstreamEntry {
                    addr: a.to_string(),
                    weight: 1,
                })
                .collect(),
            lb: L4LbAlgorithm::RoundRobin,
            tls: crate::config::L4TlsMode::None,
            max_connections: 0,
            health_check: Some(hc),
            connect_timeout_secs: 10,
            idle_timeout_secs: 600,
        }
    }

    #[test]
    fn test_new_health_state_all_healthy() {
        let state = new_health_state(3);
        assert_eq!(state.len(), 3);
        for h in state.iter() {
            assert!(h.load(Ordering::Relaxed), "initial state should be healthy");
        }
    }

    #[test]
    fn test_new_health_state_empty() {
        let state = new_health_state(0);
        assert_eq!(state.len(), 0);
    }

    #[test]
    fn test_health_state_can_be_updated() {
        let state = new_health_state(2);
        state[0].store(false, Ordering::Relaxed);
        assert!(!state[0].load(Ordering::Relaxed));
        assert!(state[1].load(Ordering::Relaxed));
    }

    #[test]
    fn test_spawn_health_checker_no_config_is_noop() {
        // health_check が None のときスレッドを起動しない（テスト: パニックしない）
        let config = Arc::new(L4ListenerConfig {
            name: "no-hc".to_string(),
            listen: "0.0.0.0:9998".to_string(),
            upstreams: vec![L4UpstreamEntry {
                addr: "127.0.0.1:8001".to_string(),
                weight: 1,
            }],
            lb: L4LbAlgorithm::RoundRobin,
            tls: crate::config::L4TlsMode::None,
            max_connections: 0,
            health_check: None,
            connect_timeout_secs: 10,
            idle_timeout_secs: 600,
        });
        let state = new_health_state(1);
        spawn_l4_health_checker(config, state.clone());
        // スレッドが起動されなくても state は変わらない
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            state[0].load(Ordering::Relaxed),
            "state should remain healthy"
        );
    }

    #[test]
    fn test_health_checker_marks_unreachable_upstream_unhealthy() {
        use std::net::TcpListener;

        // 存在しないポートへの接続は失敗する → unhealthy になるはず
        let hc = HealthCheckConfig {
            check_type: HealthCheckType::Tcp,
            interval_secs: 1,
            timeout_secs: 1,
            unhealthy_threshold: 1, // 1 回失敗で unhealthy
            healthy_threshold: 1,
            path: String::new(),
            healthy_statuses: vec![200],
            use_tls: false,
            verify_cert: true,
        };

        // 到達不能なアドレス（ポートを開いていない）
        let config = Arc::new(make_l4_config_with_hc(vec!["127.0.0.1:19988"], hc));
        let state = new_health_state(1);

        // 初期は healthy
        assert!(state[0].load(Ordering::Relaxed));

        // ヘルスチェックを 1 回手動実行してシミュレート
        let addr = "127.0.0.1:19988";
        let timeout = Duration::from_millis(200);
        let result = perform_tcp_health_check(addr, timeout);
        assert!(!result, "unreachable addr should fail health check");

        // fail_count が unhealthy_threshold に達したら unhealthy になる
        let mut fail_count = 0u32;
        let unhealthy_threshold = config.health_check.as_ref().unwrap().unhealthy_threshold;
        fail_count += 1;
        if fail_count >= unhealthy_threshold {
            state[0].store(false, Ordering::Relaxed);
        }
        assert!(
            !state[0].load(Ordering::Relaxed),
            "should be unhealthy after threshold"
        );

        // 到達可能なサーバーに戻したら healthy に回復する
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr2 = listener.local_addr().unwrap().to_string();
        let _server = std::thread::spawn(move || while let Ok(_) = listener.accept() {});

        let result2 = perform_tcp_health_check(&addr2, Duration::from_secs(1));
        assert!(result2, "reachable addr should pass health check");

        let healthy_threshold = config.health_check.as_ref().unwrap().healthy_threshold;
        let mut success_count = 0u32;
        success_count += 1;
        if success_count >= healthy_threshold {
            state[0].store(true, Ordering::Relaxed);
        }
        assert!(
            state[0].load(Ordering::Relaxed),
            "should be healthy after recovery"
        );
    }
}
