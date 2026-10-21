// F-198: tokio 互換レイヤー。設計は docs/backlog/features/F-198-tokio-free-compat-layer.md、互換表は docs/guide/library.md。
//! A tokio-compatible API layer (requires the `tokio-compat` feature; intended for the API library).
//!
//! It does not depend on tokio. Instead it provides the shape of tokio's API on top of veil's own
//! runtime (io_uring or the readiness reactor). Use it as `use veil::tokio as tokio;`, or patch the
//! `tokio` package in your dependency tree with the `tokio-shim` crate through `[patch.crates-io]`.
//!
//! - Wake-ups from other threads are forwarded to the owning thread; veil's runtime is unchanged.
//! - A task runs on the thread that spawned it (there is no work stealing). Use an I/O object on the
//!   thread that created it.
//! - `process`, runtime metrics and task dumps are not supported.

pub mod fs;
pub mod future;
pub mod io;
#[doc(hidden)]
pub mod macros;
pub mod net;
pub mod runtime;
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

/// Re-export of `#[tokio::main]` / `#[tokio::test]` (from tokio-macros). When you use `veil::tokio`
/// directly, name the crate explicitly, for example `#[veil::tokio::main(crate = "veil::tokio")]`.
pub use tokio_macros::{main, test};

/// Equivalent of `tokio::pin!`: pins a variable in place.
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
