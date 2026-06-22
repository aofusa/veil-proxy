//! レジリエンス機構（F-06）
//!
//! - サーキットブレーカー（Closed -> Open -> HalfOpen -> Closed）
//! - スライディングウィンドウ（失敗率の計測）
//! - リトライポリシー（設定は config.rs の RetryPolicy）
//! - パッシブ異常検知（Outlier Detection、config.rs の UpstreamServer で実装）
//!
//! データプレーン上で動作するため tokio に依存しない。
//! 状態は `std::sync::Mutex` と Atomic で保護する（ハンドシェイク後の
//! ホットパスでは可能な限り Atomic のみで判定する）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::CircuitBreakerConfig;

/// 一定時間ウィンドウ内の成功/失敗を記録するリングバッファ
///
/// `(timestamp, success)` を保持し、ウィンドウ外の古いエントリは
/// 集計時に破棄する。
#[derive(Debug)]
pub struct SlidingWindow {
    window: Duration,
    /// (記録時刻, 成功か)
    entries: VecDeque<(Instant, bool)>,
}

impl SlidingWindow {
    /// 指定したウィンドウ幅で作成
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            entries: VecDeque::new(),
        }
    }

    /// ウィンドウ外の古いエントリを削除
    fn evict_old(&mut self, now: Instant) {
        while let Some((ts, _)) = self.entries.front() {
            if now.duration_since(*ts) > self.window {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }

    /// 結果を記録する
    pub fn record(&mut self, success: bool) {
        let now = Instant::now();
        self.evict_old(now);
        self.entries.push_back((now, success));
    }

    /// ウィンドウ内の合計サンプル数
    pub fn total(&self) -> usize {
        let now = Instant::now();
        self.entries
            .iter()
            .filter(|(ts, _)| now.duration_since(*ts) <= self.window)
            .count()
    }

    /// ウィンドウ内の失敗数
    pub fn failures(&self) -> usize {
        let now = Instant::now();
        self.entries
            .iter()
            .filter(|(ts, s)| now.duration_since(*ts) <= self.window && !*s)
            .count()
    }

    /// 全エントリをクリア
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// サーキットブレーカーの状態
#[derive(Debug)]
pub enum CircuitState {
    /// 通常稼働。失敗をスライディングウィンドウで計測する。
    Closed {
        failures: u32,
        recent_window: SlidingWindow,
    },
    /// 遮断中。一定時間経過後に HalfOpen へ遷移する。
    Open { opened_at: Instant },
    /// 試験稼働中。限られたプローブのみ許可する。
    HalfOpen { probe_count: u32, successes: u32 },
}

/// サーキットブレーカーの統計（メトリクス用）
#[derive(Debug, Default)]
pub struct CircuitBreakerStats {
    /// Open へ遷移した累計回数
    pub open_count: AtomicU64,
    /// 成功累計
    pub success_count: AtomicU64,
    /// 失敗累計
    pub failure_count: AtomicU64,
    /// 現在の状態（0=Closed, 1=Open, 2=HalfOpen）メトリクスゲージ用
    pub state_code: AtomicU64,
}

/// サーキットブレーカー（アップストリームサーバー単位）
#[derive(Clone)]
pub struct CircuitBreaker {
    state: Arc<Mutex<CircuitState>>,
    config: CircuitBreakerConfig,
    stats: Arc<CircuitBreakerStats>,
}

impl std::fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreaker")
            .field("config", &self.config)
            .finish()
    }
}

impl CircuitBreaker {
    /// 設定からサーキットブレーカーを作成
    pub fn new(config: CircuitBreakerConfig) -> Self {
        let window = SlidingWindow::new(Duration::from_secs(config.failure_window_secs));
        Self {
            state: Arc::new(Mutex::new(CircuitState::Closed {
                failures: 0,
                recent_window: window,
            })),
            config,
            stats: Arc::new(CircuitBreakerStats::default()),
        }
    }

    /// 統計への参照
    pub fn stats(&self) -> &Arc<CircuitBreakerStats> {
        &self.stats
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, CircuitState> {
        match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn set_state_code(&self, code: u64) {
        self.stats.state_code.store(code, Ordering::Relaxed);
    }

    /// リクエストを許可するか（select 時に呼ぶ）
    ///
    /// Open で待機時間を過ぎていれば HalfOpen へ遷移してプローブを許可する。
    pub fn allow_request(&self) -> bool {
        let mut state = self.lock_state();
        match &mut *state {
            CircuitState::Closed { .. } => true,
            CircuitState::Open { opened_at } => {
                let elapsed = opened_at.elapsed();
                if elapsed >= Duration::from_secs(self.config.open_duration_secs) {
                    // HalfOpen へ遷移し、最初のプローブを許可
                    *state = CircuitState::HalfOpen {
                        probe_count: 1,
                        successes: 0,
                    };
                    self.set_state_code(2);
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen { probe_count, .. } => {
                if *probe_count < self.config.half_open_probes {
                    *probe_count += 1;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// 現在 Open（遮断中）かどうか（純粋な参照、状態遷移はしない）
    pub fn is_open(&self) -> bool {
        matches!(&*self.lock_state(), CircuitState::Open { .. })
    }

    /// リクエスト成功を記録
    pub fn record_success(&self) {
        self.stats.success_count.fetch_add(1, Ordering::Relaxed);
        let mut state = self.lock_state();
        match &mut *state {
            CircuitState::Closed {
                failures,
                recent_window,
            } => {
                *failures = 0;
                recent_window.record(true);
            }
            CircuitState::HalfOpen { successes, .. } => {
                *successes += 1;
                if *successes >= self.config.success_threshold {
                    // Closed へ復帰
                    *state = CircuitState::Closed {
                        failures: 0,
                        recent_window: SlidingWindow::new(Duration::from_secs(
                            self.config.failure_window_secs,
                        )),
                    };
                    self.set_state_code(0);
                }
            }
            CircuitState::Open { .. } => {
                // Open 中の成功は通常発生しないが無視
            }
        }
    }

    /// リクエスト失敗を記録
    pub fn record_failure(&self) {
        self.stats.failure_count.fetch_add(1, Ordering::Relaxed);
        let mut state = self.lock_state();
        match &mut *state {
            CircuitState::Closed {
                failures,
                recent_window,
            } => {
                recent_window.record(false);
                *failures += 1;
                // ウィンドウ内失敗数が閾値を超えたら Open
                let window_failures = recent_window.failures() as u32;
                if *failures >= self.config.failure_threshold
                    || window_failures >= self.config.failure_threshold
                {
                    *state = CircuitState::Open {
                        opened_at: Instant::now(),
                    };
                    self.stats.open_count.fetch_add(1, Ordering::Relaxed);
                    self.set_state_code(1);
                }
            }
            CircuitState::HalfOpen { .. } => {
                // HalfOpen 中の失敗は即 Open へ
                *state = CircuitState::Open {
                    opened_at: Instant::now(),
                };
                self.stats.open_count.fetch_add(1, Ordering::Relaxed);
                self.set_state_code(1);
            }
            CircuitState::Open { .. } => {}
        }
    }

    /// 現在の状態コード（0=Closed,1=Open,2=HalfOpen）
    pub fn state_code(&self) -> u64 {
        match &*self.lock_state() {
            CircuitState::Closed { .. } => 0,
            CircuitState::Open { .. } => 1,
            CircuitState::HalfOpen { .. } => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 3,
            failure_window_secs: 60,
            open_duration_secs: 1,
            half_open_probes: 2,
            success_threshold: 2,
            trip_on_timeout: true,
        }
    }

    #[test]
    fn sliding_window_counts() {
        let mut w = SlidingWindow::new(Duration::from_secs(10));
        w.record(true);
        w.record(false);
        w.record(false);
        assert_eq!(w.total(), 3);
        assert_eq!(w.failures(), 2);
        w.clear();
        assert_eq!(w.total(), 0);
    }

    #[test]
    fn closed_to_open_on_threshold() {
        let cb = CircuitBreaker::new(test_config());
        assert!(cb.allow_request());
        assert_eq!(cb.state_code(), 0);
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state_code(), 0); // まだ閾値未満
        cb.record_failure();
        assert_eq!(cb.state_code(), 1); // Open
        assert!(!cb.allow_request()); // 遮断中
    }

    #[test]
    fn open_to_halfopen_to_closed() {
        let cb = CircuitBreaker::new(test_config());
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_open());
        // open_duration_secs=1 経過を待つ
        std::thread::sleep(Duration::from_millis(1100));
        // allow_request で HalfOpen へ遷移
        assert!(cb.allow_request());
        assert_eq!(cb.state_code(), 2);
        // success_threshold=2 の成功で Closed
        cb.record_success();
        cb.record_success();
        assert_eq!(cb.state_code(), 0);
    }

    #[test]
    fn halfopen_failure_reopens() {
        let cb = CircuitBreaker::new(test_config());
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        std::thread::sleep(Duration::from_millis(1100));
        assert!(cb.allow_request()); // HalfOpen
        cb.record_failure(); // 即 Open
        assert!(cb.is_open());
    }

    #[test]
    fn stats_tracked() {
        let cb = CircuitBreaker::new(test_config());
        cb.record_success();
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.stats().success_count.load(Ordering::Relaxed), 1);
        assert!(cb.stats().failure_count.load(Ordering::Relaxed) >= 3);
        assert_eq!(cb.stats().open_count.load(Ordering::Relaxed), 1);
    }
}
