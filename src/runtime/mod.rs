//! カスタム io_uring ランタイム
//!
//! monoio を削除して自前の io_uring 実装に置き換えたランタイムモジュール。
//!
//! ## 設計方針
//!
//! - **ライブラリは最小限**: `libc`（既存依存）と `bytes`（既存依存）のみ使用
//! - **ゼロコピー**: bytes クレートを活用してホットパスでのコピーを排除
//! - **ホットパスでのメモリアロケーション禁止**: 既存のバッファプールを最大限活用
//! - **ホットパスでの同期呼び出し禁止**: 全 I/O は非同期
//! - **io_uring IORING_REGISTER_RESTRICTIONS**: 許可オペコードを制限してセキュリティ強化
//! - **スレッドごとに io_uring リングを持つ** thread-per-core 設計
//!
//! ## モジュール構成
//!
//! - `buf` - IoBuf/IoBufMut トレイト（バックエンド共有）
//! - `io` - I/O トレイト・File（バックエンド共有）
//! - `offload` - ブロッキング処理のワーカースレッド退避（バックエンド共有）
//! - `uring` - io_uring バックエンド（`veil_rt_uring`）:
//!   - `ring` - io_uring リング管理（setup/enter/register、SQE/CQE raw 操作）
//!   - `executor` - シングルスレッド非同期エグゼキュータ
//!   - `tcp` - 非同期 TcpListener/TcpStream
//!   - `timer` - タイムアウト管理
//!   - `splice` - splice(2) 経由のゼロコピー転送
//!
//! バックエンドの選択は build.rs が発行する cfg（`veil_rt_uring` /
//! `veil_rt_reactor`）で行い、`runtime::tcp` 等の公開パスはどちらの
//! バックエンドでも不変に保つ（F-120）。`veil_rt_reactor`（Linux では
//! `--features epoll`）は readiness ベースの `reactor` バックエンドへ切り替える
//! （Phase 2。poller は現状 epoll のみ、kqueue は Phase 4 で追加予定）。

pub mod buf;
pub mod io;
pub mod offload;
// L4 UDP プロキシ専用の汎用 UDP ソケット（F-124）。`l4-proxy` feature でのみ使用するため
// dead_code 警告を避けるべくゲートする。
#[cfg(feature = "l4-proxy")]
pub mod udp;

#[cfg(veil_rt_reactor)]
mod reactor;
#[cfg(veil_rt_uring)]
mod uring;

#[cfg(veil_rt_reactor)]
pub use reactor::{executor, tcp, timer};
// splice(2) は Linux 専用。BSD（kqueue reactor）では `runtime::splice` パス自体を提供せず、
// 消費側（L4 / kTLS proxy）が `cfg(target_os = "linux")` でバッファプール経由の
// read/write 転送へフォールバックする（F-120 設計 3.3 節）。
#[cfg(all(veil_rt_reactor, target_os = "linux"))]
pub use reactor::splice;
#[cfg(veil_rt_uring)]
pub use uring::{executor, ring, splice, tcp, timer};

// 公開 API の再エクスポート
pub use buf::{IoBuf, IoBufMut};
pub use executor::{spawn, yield_now, Executor, TaskPool};
#[cfg(veil_rt_uring)]
pub use ring::IoUring;
pub use tcp::{TcpListener, TcpStream};
pub use timer::{sleep, timeout, Elapsed};
#[cfg(feature = "l4-proxy")]
pub use udp::UdpSocket;

/// 現在のスレッドにランタイムドライバ（io_uring リング、または reactor の poller）が
/// 初期化済みかを判定する、バックエンド中立な API。
///
/// `runtime::offload`（F-29）が、ドライバのあるワーカースレッドでは非同期待機を、
/// ドライバの無いコンテキスト（単体テスト等）では同期インライン実行をするための
/// 分岐に使う。
#[cfg(veil_rt_uring)]
pub fn has_driver() -> bool {
    executor::has_ring()
}

/// `has_driver` の reactor 版（epoll poller の初期化有無で判定する）。
#[cfg(veil_rt_reactor)]
pub fn has_driver() -> bool {
    executor::has_driver()
}

use std::future::Future;

// ====================
// デフォルトリングエントリ数
// ====================

/// デフォルトの io_uring リングエントリ数（2 の冪数）
pub const DEFAULT_RING_ENTRIES: u32 = 4096;

// ====================
// ランタイム設定
// ====================

/// ランタイム設定
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// io_uring リングエントリ数（デフォルト: 4096）
    pub ring_entries: u32,
    /// SQPOLL 有効化（カーネルがSQを自動ポーリング）
    pub sqpoll: bool,
    /// SQPOLL スレッドのアイドルタイムアウト（ミリ秒）
    pub sqpoll_idle_ms: u32,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            ring_entries: DEFAULT_RING_ENTRIES,
            sqpoll: false,
            sqpoll_idle_ms: 2000,
        }
    }
}

// ====================
// スレッドランタイム起動
// ====================

/// 現在のスレッドでランタイムを初期化して Future を実行する
///
/// monoio の `RuntimeBuilder::new_current_thread().build()?.block_on(...)` に相当。
///
/// # 使用例
///
/// ```rust,ignore
/// runtime::block_on(async move {
///     // io_uring を使った非同期処理
/// });
/// ```
pub fn block_on<F, R>(future: F) -> R
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    block_on_with_config(future, &RuntimeConfig::default())
}

/// 設定付きでランタイムを初期化して Future を実行する
pub fn block_on_with_config<F, R>(future: F, config: &RuntimeConfig) -> R
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    block_on_with_config_backend(future, config)
}

/// バックエンド依存の `block_on_with_config` 実装（io_uring 版）。
///
/// `ring::IORING_SETUP_SQPOLL` 等、io_uring 固有のセットアップはここに閉じ込め、
/// reactor バックエンド追加時（Phase 2）はここに同名の reactor 版を追加する
/// （`config.ring_entries` / `config.sqpoll` は reactor では無視する契約）。
#[cfg(veil_rt_uring)]
fn block_on_with_config_backend<F, R>(future: F, config: &RuntimeConfig) -> R
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    // io_uring フラグを構築
    let mut flags = 0u32;
    if config.sqpoll {
        flags |= ring::IORING_SETUP_SQPOLL;
    }

    // スレッドローカル io_uring リングを初期化
    executor::init_ring(config.ring_entries, flags).expect("Failed to initialize io_uring ring");

    // スレッドローカルエグゼキュータを初期化
    executor::init_executor();

    // spawn() と同じスレッドローカルエグゼキュータ（同一キュー）上で実行する。
    // 別途 Executor::new() で作ったエグゼキュータを使うと、spawn() されたタスクが
    // スレッドローカル側のキューに積まれて永遠にポーリングされない（接続を accept
    // しても handle_connection が動かず応答できなくなる）。
    let exec = executor::current_executor();
    exec.block_on(future)
}

/// バックエンド依存の `block_on_with_config` 実装（reactor 版）。
///
/// `config.ring_entries` / `config.sqpoll` は reactor では無視する（io_uring 固有設定の
/// ため）。poller（epoll インスタンス）の初期化のみ行う。
#[cfg(veil_rt_reactor)]
fn block_on_with_config_backend<F, R>(future: F, _config: &RuntimeConfig) -> R
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    executor::init_reactor().expect("Failed to initialize reactor poller (epoll_create1)");
    executor::init_executor();

    let exec = executor::current_executor();
    exec.block_on(future)
}

// ====================
// time モジュール（monoio 互換）
// ====================

/// 時間関連ユーティリティ（monoio::time 互換）
pub mod time {
    // timer モジュールの sleep/timeout/Elapsed を re-export
    pub use super::timer::{sleep, timeout, Elapsed, Sleep};
    pub use std::time::Duration;
}
