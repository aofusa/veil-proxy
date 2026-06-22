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
//! - `ring` - io_uring リング管理（setup/enter/register、SQE/CQE raw 操作）
//! - `executor` - シングルスレッド非同期エグゼキュータ
//! - `tcp` - 非同期 TcpListener/TcpStream
//! - `timer` - タイムアウト管理
//! - `buf` - IoBuf/IoBufMut トレイト

pub mod buf;
pub mod executor;
pub mod ring;
pub mod tcp;
pub mod timer;

// 公開 API の再エクスポート
pub use buf::{IoBuf, IoBufMut};
pub use executor::{spawn, Executor};
pub use ring::IoUring;
pub use tcp::{TcpListener, TcpStream};
pub use timer::{sleep, timeout, Elapsed};

use std::future::Future;
use std::time::Duration;

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
    // io_uring フラグを構築
    let mut flags = 0u32;
    if config.sqpoll {
        flags |= ring::IORING_SETUP_SQPOLL;
    }

    // スレッドローカル io_uring リングを初期化
    executor::init_ring(config.ring_entries, flags)
        .expect("Failed to initialize io_uring ring");

    // スレッドローカルエグゼキュータを初期化
    executor::init_executor();

    // エグゼキュータを作成して Future を実行
    let exec = Executor::new();
    exec.block_on(future)
}

// ====================
// time モジュール（monoio 互換）
// ====================

/// 時間関連ユーティリティ（monoio::time 互換）
pub mod time {
    pub use super::timer::{sleep, timeout, Elapsed, Sleep};
    pub use std::time::Duration;

    /// タイムアウト付きで Future を実行する（monoio::time::timeout 互換）
    pub async fn timeout<F, R>(
        duration: Duration,
        future: F,
    ) -> Result<R, super::timer::Elapsed>
    where
        F: std::future::Future<Output = R>,
    {
        super::timer::timeout(duration, future).await
    }
}
