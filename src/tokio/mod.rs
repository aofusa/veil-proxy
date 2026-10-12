//! tokio 互換レイヤー（F-198、`tokio-compat` feature。API ライブラリ専用）。
//!
//! tokio に依存せず、veil のランタイム（io_uring / reactor）の上に tokio の API の形を提供する。
//! `use veil::tokio as tokio;` で使うか、`crates/veil-tokio/tokio-shim`（パッケージ名 `tokio`）を
//! `[patch.crates-io]` に当てて依存ツリーの tokio を置き換える。
//!
//! - スレッド越しの wake は `waker` の受け口で所有スレッドへ返す（veil のランタイムは変えない）。
//! - タスクは spawn したスレッドで動く（work stealing なし）。I/O オブジェクトも作ったスレッドで使う。
//! - 時計の停止（test-util の `pause`/`advance`）・`process` は未対応。
//!
//! 設計: docs/backlog/features/F-198-tokio-free-compat-layer.md。互換表: docs/guide/library.md。

pub mod fs;
pub mod future;
pub mod io;
#[doc(hidden)]
pub mod macros;
#[cfg(unix)]
pub mod net;
pub mod runtime;
#[cfg(unix)]
pub mod signal;
pub mod sync;
pub mod task;
pub mod time;
pub(crate) mod waker;

pub use task::spawn;

pub use crate::__veil_tokio_join as join;
pub use crate::__veil_tokio_pin as pin;
pub use crate::__veil_tokio_select as select;
pub use crate::__veil_tokio_task_local as task_local;
pub use crate::__veil_tokio_try_join as try_join;

/// `#[tokio::main]` / `#[tokio::test]`（tokio-macros。`veil::tokio` を直接使うときは
/// `#[veil::tokio::main(crate = "veil::tokio")]` のように `crate` を指定する）。
pub use tokio_macros::{main, test};

/// `tokio::pin!`（変数をその場でピン留めする）。
#[macro_export]
#[doc(hidden)]
macro_rules! __veil_tokio_pin {
    ($($x:ident),* $(,)?) => { $(
        let mut $x = $x;
        #[allow(unused_mut)]
        let mut $x = unsafe {
            // SAFETY: 元の変数は同名の束縛で隠れ、以後動かせない
            ::std::pin::Pin::new_unchecked(&mut $x)
        };
    )* };
    ($(let $x:ident = $init:expr;)*) => { $(
        let $x = $init;
        $crate::__veil_tokio_pin!($x);
    )* };
}
