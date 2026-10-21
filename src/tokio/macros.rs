// F-198: 生成元は docs/backlog/features/F-198-tokio-free-compat-layer.md の「結果」。
//! `select!`, `join!` and `try_join!`.
//!
//! The `macro_rules` of tokio 1.53.2 (`src/macros/{select,join,try_join}.rs`) are taken over as they are,
//! with only the internal paths replaced by `$crate::tokio::macros::support` and `__veil_tokio_*`
//! (MIT License, Copyright (c) Tokio Contributors). The parsing of `select!` branches uses the
//! runtime-independent procedural macro crate `tokio-macros`.
#![allow(clippy::all)]

/// Helpers used by the macros (same names as tokio's `macros::support`).
#[doc(hidden)]
pub mod support {
    pub use super::{BiasedRotator, Rotator, RotatorSelect, SelectBiased, SelectNormal};
    pub use crate::tokio::future::maybe_done::maybe_done;
    pub use std::future::poll_fn;
    pub use std::future::{Future, IntoFuture};
    pub use std::pin::Pin;
    pub use std::result::Result;
    pub use std::task::{ready, Context, Poll};
    pub use tokio_macros::{select_priv_clean_pattern, select_priv_declare_output_enum};

    /// A random number in `0..n` (for the fairness of `select!`).
    #[doc(hidden)]
    pub fn thread_rng_n(n: u32) -> u32 {
        use std::cell::Cell;
        thread_local! {
            static RNG: Cell<u64> = const { Cell::new(0) };
        }
        RNG.with(|r| {
            let mut x = r.get();
            if x == 0 {
                x = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(1)
                    ^ (r as *const _ as u64))
                    | 1;
            }
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            r.set(x);
            ((x >> 32) * n as u64 >> 32) as u32
        })
    }

    #[doc(hidden)]
    #[inline]
    pub fn poll_budget_available(_: &mut Context<'_>) -> Poll<()> {
        Poll::Ready(())
    }
}

#[macro_export]
#[doc(hidden)]
macro_rules! __veil_tokio_select {
    // Uses a declarative macro to do **most** of the work. While it is possible
    // to implement fully with a declarative macro, a procedural macro is used
    // to enable improved error messages.
    //
    // The macro is structured as a tt-muncher. All branches are processed and
    // normalized. Once the input is normalized, it is passed to the top-most
    // rule. When entering the macro, `@{ }` is inserted at the front. This is
    // used to collect the normalized input.
    //
    // The macro only recurses once per branch. This allows using `select!`
    // without requiring the user to increase the recursion limit.

    // All input is normalized, now transform.
    (@ {
        // The index of the future to poll first (in bias mode), or the RNG
        // expression to use to pick a future to poll first.
        start=$start:expr;

        // One `_` for each branch in the `select!` macro. Passing this to
        // `count!` converts $skip to an integer.
        ( $($count:tt)* )

        // Normalized select branches. `( $skip )` is a set of `_` characters.
        // There is one `_` for each select branch **before** this one. Given
        // that all input futures are stored in a tuple, $skip is useful for
        // generating a pattern to reference the future for the current branch.
        // $skip is also used as an argument to `count!`, returning the index of
        // the current select branch.
        $( ( $($skip:tt)* ) $bind:pat = $fut:expr, if $c:expr => $handle:expr, )+

        // Fallback expression used when all select branches have been disabled.
        ; $else:expr

    }) => {{
        // Enter a context where stable "function-like" proc macros can be used.
        //
        // This module is defined within a scope and should not leak out of this
        // macro.
        #[doc(hidden)]
        mod __tokio_select_util {
            // Generate an enum with one variant per select branch
            $crate::tokio::macros::support::select_priv_declare_output_enum!( ( $($count)* ) );
        }

        // `tokio::macros::support` is a public, but doc(hidden) module
        // including a re-export of all types needed by this macro.
        use $crate::tokio::macros::support::Pin;

        const BRANCHES: u32 = $crate::__veil_tokio_count!( $($count)* );

        let mut disabled: __tokio_select_util::Mask = Default::default();

        // First, invoke all the pre-conditions. For any that return true,
        // set the appropriate bit in `disabled`.
        $(
            if !$c {
                let mask: __tokio_select_util::Mask = 1 << $crate::__veil_tokio_count!( $($skip)* );
                disabled |= mask;
            }
        )*

        // Create a scope to separate polling from handling the output. This
        // adds borrow checker flexibility when using the macro.
        let mut output = {
            // Store each future directly first (that is, without wrapping the future in a call to
            // `IntoFuture::into_future`). This allows the `$fut` expression to make use of
            // temporary lifetime extension.
            //
            // https://doc.rust-lang.org/1.58.1/reference/destructors.html#temporary-lifetime-extension
            let futures_init = ($( $fut, )+);

            // Safety: Nothing must be moved out of `futures`. This is to
            // satisfy the requirement of `Pin::new_unchecked` called below.
            //
            // We can't use the `pin!` macro for this because `futures` is a
            // tuple and the standard library provides no way to pin-project to
            // the fields of a tuple.
            let mut futures = ($( $crate::tokio::macros::support::IntoFuture::into_future(
                        $crate::__veil_tokio_count_field!( futures_init.$($skip)* )
            ),)+);

            // This assignment makes sure that the `poll_fn` closure only has a
            // reference to the futures, instead of taking ownership of them.
            // This mitigates the issue described in
            // <https://internals.rust-lang.org/t/surprising-soundness-trouble-around-pollfn/17484>
            let mut futures = &mut futures;

            $crate::tokio::macros::support::poll_fn(|cx| {
                // Return `Pending` when the task budget is depleted since budget-aware futures
                // are going to yield anyway and other futures will not cooperate.
                $crate::tokio::macros::support::ready!($crate::tokio::macros::support::poll_budget_available(cx));

                // Track if any branch returns pending. If no branch completes
                // **or** returns pending, this implies that all branches are
                // disabled.
                let mut is_pending = false;

                // Choose a starting index to begin polling the futures at. In
                // practice, this will either be a pseudo-randomly generated
                // number by default, or the constant 0 if `biased;` is
                // supplied.
                let start = $start;

                for i in 0..BRANCHES {
                    let branch;
                    #[allow(clippy::modulo_one)]
                    {
                        branch = (start + i) % BRANCHES;
                    }
                    match branch {
                        $(
                            #[allow(unreachable_code)]
                            $crate::__veil_tokio_count!( $($skip)* ) => {
                                // First, if the future has previously been
                                // disabled, do not poll it again. This is done
                                // by checking the associated bit in the
                                // `disabled` bit field.
                                let mask = 1 << branch;

                                if disabled & mask == mask {
                                    // The future has been disabled.
                                    continue;
                                }

                                // Extract the future for this branch from the
                                // tuple
                                let ( $($skip,)* fut, .. ) = &mut *futures;

                                // Safety: future is stored on the stack above
                                // and never moved.
                                let mut fut = unsafe { $crate::tokio::macros::support::Pin::new_unchecked(fut) };

                                // Try polling it
                                let out = match $crate::tokio::macros::support::Future::poll(fut, cx) {
                                    $crate::tokio::macros::support::Poll::Ready(out) => out,
                                    $crate::tokio::macros::support::Poll::Pending => {
                                        // Track that at least one future is
                                        // still pending and continue polling.
                                        is_pending = true;
                                        continue;
                                    }
                                };

                                // Disable the future from future polling.
                                disabled |= mask;

                                // The future returned a value, check if matches
                                // the specified pattern.
                                #[allow(unused_variables)]
                                #[allow(unused_mut)]
                                match &out {
                                    $crate::tokio::macros::support::select_priv_clean_pattern!($bind) => {}
                                    _ => continue,
                                }

                                // The select is complete, return the value
                                return $crate::tokio::macros::support::Poll::Ready($crate::__veil_tokio_select_variant!(__tokio_select_util::Out, ($($skip)*))(out));
                            }
                        )*
                        _ => unreachable!("reaching this means there probably is an off by one bug"),
                    }
                }

                if is_pending {
                    $crate::tokio::macros::support::Poll::Pending
                } else {
                    // All branches have been disabled.
                    $crate::tokio::macros::support::Poll::Ready(__tokio_select_util::Out::Disabled)
                }
            }).await
        };

        match output {
            $(
                $crate::__veil_tokio_select_variant!(__tokio_select_util::Out, ($($skip)*) ($bind)) => $handle,
            )*
            __tokio_select_util::Out::Disabled => $else,
            _ => unreachable!("failed to match bind"),
        }
    }};

    // ==== Normalize =====

    // These rules match a single `select!` branch and normalize it for
    // processing by the first rule.

    (@ { start=$start:expr; $($t:tt)* } ) => {
        // No `else` branch
        $crate::__veil_tokio_select!(@{ start=$start; $($t)*; panic!("all branches are disabled and there is no else branch") })
    };
    (@ { start=$start:expr; $($t:tt)* } else => $else:expr $(,)?) => {
        $crate::__veil_tokio_select!(@{ start=$start; $($t)*; $else })
    };
    (@ { start=$start:expr; ( $($s:tt)* ) $($t:tt)* } $p:pat = $f:expr, if $c:expr => $h:block, $($r:tt)* ) => {
        $crate::__veil_tokio_select!(@{ start=$start; ($($s)* _) $($t)* ($($s)*) $p = $f, if $c => $h, } $($r)*)
    };
    (@ { start=$start:expr; ( $($s:tt)* ) $($t:tt)* } $p:pat = $f:expr => $h:block, $($r:tt)* ) => {
        $crate::__veil_tokio_select!(@{ start=$start; ($($s)* _) $($t)* ($($s)*) $p = $f, if true => $h, } $($r)*)
    };
    (@ { start=$start:expr; ( $($s:tt)* ) $($t:tt)* } $p:pat = $f:expr, if $c:expr => $h:block $($r:tt)* ) => {
        $crate::__veil_tokio_select!(@{ start=$start; ($($s)* _) $($t)* ($($s)*) $p = $f, if $c => $h, } $($r)*)
    };
    (@ { start=$start:expr; ( $($s:tt)* ) $($t:tt)* } $p:pat = $f:expr => $h:block $($r:tt)* ) => {
        $crate::__veil_tokio_select!(@{ start=$start; ($($s)* _) $($t)* ($($s)*) $p = $f, if true => $h, } $($r)*)
    };
    (@ { start=$start:expr; ( $($s:tt)* ) $($t:tt)* } $p:pat = $f:expr, if $c:expr => $h:expr ) => {
        $crate::__veil_tokio_select!(@{ start=$start; ($($s)* _) $($t)* ($($s)*) $p = $f, if $c => $h, })
    };
    (@ { start=$start:expr; ( $($s:tt)* ) $($t:tt)* } $p:pat = $f:expr => $h:expr ) => {
        $crate::__veil_tokio_select!(@{ start=$start; ($($s)* _) $($t)* ($($s)*) $p = $f, if true => $h, })
    };
    (@ { start=$start:expr; ( $($s:tt)* ) $($t:tt)* } $p:pat = $f:expr, if $c:expr => $h:expr, $($r:tt)* ) => {
        $crate::__veil_tokio_select!(@{ start=$start; ($($s)* _) $($t)* ($($s)*) $p = $f, if $c => $h, } $($r)*)
    };
    (@ { start=$start:expr; ( $($s:tt)* ) $($t:tt)* } $p:pat = $f:expr => $h:expr, $($r:tt)* ) => {
        $crate::__veil_tokio_select!(@{ start=$start; ($($s)* _) $($t)* ($($s)*) $p = $f, if true => $h, } $($r)*)
    };

    // ===== Entry point =====

    ($(biased;)? else => $else:expr $(,)? ) => {{
        $else
    }};

    (biased; $p:pat = $($t:tt)* ) => {
        $crate::__veil_tokio_select!(@{ start=0; () } $p = $($t)*)
    };

    ( $p:pat = $($t:tt)* ) => {
        // Randomly generate a starting point. This makes `select!` a bit more
        // fair and avoids always polling the first future.
        $crate::__veil_tokio_select!(@{ start={ $crate::tokio::macros::support::thread_rng_n(BRANCHES) }; () } $p = $($t)*)
    };

    () => {
        compile_error!("select! requires at least one branch.")
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! __veil_tokio_count {
    () => {
        0
    };
    (_) => {
        1
    };
    (_ _) => {
        2
    };
    (_ _ _) => {
        3
    };
    (_ _ _ _) => {
        4
    };
    (_ _ _ _ _) => {
        5
    };
    (_ _ _ _ _ _) => {
        6
    };
    (_ _ _ _ _ _ _) => {
        7
    };
    (_ _ _ _ _ _ _ _) => {
        8
    };
    (_ _ _ _ _ _ _ _ _) => {
        9
    };
    (_ _ _ _ _ _ _ _ _ _) => {
        10
    };
    (_ _ _ _ _ _ _ _ _ _ _) => {
        11
    };
    (_ _ _ _ _ _ _ _ _ _ _ _) => {
        12
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _) => {
        13
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        14
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        15
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        16
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        17
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        18
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        19
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        20
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        21
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        22
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        23
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        24
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        25
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        26
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        27
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        28
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        29
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        30
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        31
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        32
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        33
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        34
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        35
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        36
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        37
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        38
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        39
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        40
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        41
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        42
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        43
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        44
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        45
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        46
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        47
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        48
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        49
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        50
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        51
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        52
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        53
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        54
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        55
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        56
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        57
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        58
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        59
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        60
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        61
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        62
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        63
    };
    (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        64
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! __veil_tokio_count_field {
    ($var:ident. ) => {
        $var.0
    };
    ($var:ident. _) => {
        $var.1
    };
    ($var:ident. _ _) => {
        $var.2
    };
    ($var:ident. _ _ _) => {
        $var.3
    };
    ($var:ident. _ _ _ _) => {
        $var.4
    };
    ($var:ident. _ _ _ _ _) => {
        $var.5
    };
    ($var:ident. _ _ _ _ _ _) => {
        $var.6
    };
    ($var:ident. _ _ _ _ _ _ _) => {
        $var.7
    };
    ($var:ident. _ _ _ _ _ _ _ _) => {
        $var.8
    };
    ($var:ident. _ _ _ _ _ _ _ _ _) => {
        $var.9
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _) => {
        $var.10
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _) => {
        $var.11
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.12
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.13
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.14
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.15
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.16
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.17
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.18
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.19
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.20
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.21
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.22
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.23
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.24
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.25
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.26
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.27
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.28
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.29
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.30
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.31
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.32
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.33
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.34
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.35
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.36
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.37
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.38
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.39
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.40
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.41
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.42
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.43
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.44
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.45
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.46
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.47
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.48
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.49
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.50
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.51
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.52
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.53
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.54
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.55
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.56
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.57
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.58
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.59
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.60
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.61
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.62
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.63
    };
    ($var:ident. _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) => {
        $var.64
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! __veil_tokio_select_variant {
    ($($p:ident)::*, () $($t:tt)*) => {
        $($p)::*::_0 $($t)*
    };
    ($($p:ident)::*, (_) $($t:tt)*) => {
        $($p)::*::_1 $($t)*
    };
    ($($p:ident)::*, (_ _) $($t:tt)*) => {
        $($p)::*::_2 $($t)*
    };
    ($($p:ident)::*, (_ _ _) $($t:tt)*) => {
        $($p)::*::_3 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _) $($t:tt)*) => {
        $($p)::*::_4 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_5 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_6 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_7 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_8 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_9 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_10 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_11 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_12 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_13 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_14 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_15 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_16 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_17 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_18 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_19 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_20 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_21 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_22 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_23 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_24 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_25 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_26 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_27 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_28 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_29 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_30 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_31 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_32 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_33 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_34 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_35 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_36 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_37 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_38 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_39 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_40 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_41 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_42 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_43 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_44 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_45 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_46 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_47 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_48 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_49 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_50 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_51 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_52 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_53 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_54 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_55 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_56 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_57 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_58 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_59 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_60 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_61 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_62 $($t)*
    };
    ($($p:ident)::*, (_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _) $($t:tt)*) => {
        $($p)::*::_63 $($t)*
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! __veil_tokio_join {
    (@ {
        // Type of rotator that controls which inner future to start with
        // when polling our output future.
        rotator_select=$rotator_select:ty;

        // One `_` for each branch in the `join!` macro. This is not used once
        // normalization is complete.
        ( $($count:tt)* )

        // The expression `0+1+1+ ... +1` equal to the number of branches.
        ( $($total:tt)* )

        // Normalized join! branches
        $( ( $($skip:tt)* ) $e:expr, )*

    }) => {{
        // Safety: nothing must be moved out of `futures`. This is to satisfy
        // the requirement of `Pin::new_unchecked` called below.
        //
        // We can't use the `pin!` macro for this because `futures` is a tuple
        // and the standard library provides no way to pin-project to the fields
        // of a tuple.
        let mut futures = ( $( $crate::tokio::macros::support::maybe_done($e), )* );

        // This assignment makes sure that the `poll_fn` closure only has a
        // reference to the futures, instead of taking ownership of them. This
        // mitigates the issue described in
        // <https://internals.rust-lang.org/t/surprising-soundness-trouble-around-pollfn/17484>
        let mut futures = &mut futures;

        // Each time the future created by poll_fn is polled, if not using biased mode,
        // a different future is polled first to ensure every future passed to join!
        // can make progress even if one of the futures consumes the whole budget.
        let mut rotator = <$rotator_select as $crate::tokio::macros::support::RotatorSelect>::Rotator::<{$($total)*}>::default();

        $crate::tokio::macros::support::poll_fn(move |cx| {
            const COUNT: u32 = $($total)*;

            let mut is_pending = false;
            let mut to_run = COUNT;

            // The number of futures that will be skipped in the first loop iteration.
            let mut skip = rotator.num_skip();

            // This loop runs twice and the first `skip` futures
            // are not polled in the first iteration.
            loop {
            $(
                if skip == 0 {
                    if to_run == 0 {
                        // Every future has been polled
                        break;
                    }
                    to_run -= 1;

                    // Extract the future for this branch from the tuple.
                    let ( $($skip,)* fut, .. ) = &mut *futures;

                    // Safety: future is stored on the stack above
                    // and never moved.
                    let mut fut = unsafe { $crate::tokio::macros::support::Pin::new_unchecked(fut) };

                    // Try polling
                    if $crate::tokio::macros::support::Future::poll(fut.as_mut(), cx).is_pending() {
                        is_pending = true;
                    }
                } else {
                    // Future skipped, one less future to skip in the next iteration
                    skip -= 1;
                }
            )*
            }

            if is_pending {
                $crate::tokio::macros::support::Poll::Pending
            } else {
                $crate::tokio::macros::support::Poll::Ready(($({
                    // Extract the future for this branch from the tuple.
                    let ( $($skip,)* fut, .. ) = &mut futures;

                    // Safety: future is stored on the stack above
                    // and never moved.
                    let mut fut = unsafe { $crate::tokio::macros::support::Pin::new_unchecked(fut) };

                    fut.take_output().expect("expected completed future")
                },)*))
            }
        }).await
    }};

    // ===== Normalize =====

    (@ { rotator_select=$rotator_select:ty; ( $($s:tt)* ) ( $($n:tt)* ) $($t:tt)* } $e:expr, $($r:tt)* ) => {
        $crate::__veil_tokio_join!(@{ rotator_select=$rotator_select; ($($s)* _) ($($n)* + 1) $($t)* ($($s)*) $e, } $($r)*)
    };

    // ===== Entry point =====
    ( biased; $($e:expr),+ $(,)?) => {
        $crate::__veil_tokio_join!(@{ rotator_select=$crate::tokio::macros::support::SelectBiased; () (0) } $($e,)*)
    };

    ( $($e:expr),+ $(,)?) => {
        $crate::__veil_tokio_join!(@{ rotator_select=$crate::tokio::macros::support::SelectNormal; () (0) } $($e,)*)
    };

    (biased;) => { async {}.await };

    () => { async {}.await }
}

#[macro_export]
#[doc(hidden)]
macro_rules! __veil_tokio_try_join {
    (@ {
        // Type of rotator that controls which inner future to start with
        // when polling our output future.
        rotator_select=$rotator_select:ty;

        // One `_` for each branch in the `try_join!` macro. This is not used once
        // normalization is complete.
        ( $($count:tt)* )

        // The expression `0+1+1+ ... +1` equal to the number of branches.
        ( $($total:tt)* )

        // Normalized try_join! branches
        $( ( $($skip:tt)* ) $e:expr, )*

    }) => {{
        // Safety: nothing must be moved out of `futures`. This is to satisfy
        // the requirement of `Pin::new_unchecked` called below.
        //
        // We can't use the `pin!` macro for this because `futures` is a tuple
        // and the standard library provides no way to pin-project to the fields
        // of a tuple.
        let mut futures = ( $( $crate::tokio::macros::support::maybe_done($e), )* );

        // This assignment makes sure that the `poll_fn` closure only has a
        // reference to the futures, instead of taking ownership of them. This
        // mitigates the issue described in
        // <https://internals.rust-lang.org/t/surprising-soundness-trouble-around-pollfn/17484>
        let mut futures = &mut futures;

        // Each time the future created by poll_fn is polled, if not using biased mode,
        // a different future is polled first to ensure every future passed to try_join!
        // can make progress even if one of the futures consumes the whole budget.
        let mut rotator = <$rotator_select as $crate::tokio::macros::support::RotatorSelect>::Rotator::<{$($total)*}>::default();

        $crate::tokio::macros::support::poll_fn(move |cx| {
            const COUNT: u32 = $($total)*;

            let mut is_pending = false;
            let mut to_run = COUNT;

            // The number of futures that will be skipped in the first loop iteration.
            let mut skip = rotator.num_skip();

            // This loop runs twice and the first `skip` futures
            // are not polled in the first iteration.
            loop {
            $(
                if skip == 0 {
                    if to_run == 0 {
                        // Every future has been polled
                        break;
                    }
                    to_run -= 1;

                    // Extract the future for this branch from the tuple.
                    let ( $($skip,)* fut, .. ) = &mut *futures;

                    // Safety: future is stored on the stack above
                    // and never moved.
                    let mut fut = unsafe { $crate::tokio::macros::support::Pin::new_unchecked(fut) };

                    // Try polling
                    if $crate::tokio::macros::support::Future::poll(fut.as_mut(), cx).is_pending() {
                        is_pending = true;
                    } else if fut.as_mut().output_mut().expect("expected completed future").is_err() {
                        return $crate::tokio::macros::support::Poll::Ready($crate::tokio::macros::support::Result::Err(fut.take_output().expect("expected completed future").err().unwrap()))
                    }
                } else {
                    // Future skipped, one less future to skip in the next iteration
                    skip -= 1;
                }
            )*
            }

            if is_pending {
                $crate::tokio::macros::support::Poll::Pending
            } else {
                $crate::tokio::macros::support::Poll::Ready($crate::tokio::macros::support::Result::Ok(($({
                    // Extract the future for this branch from the tuple.
                    let ( $($skip,)* fut, .. ) = &mut futures;

                    // Safety: future is stored on the stack above
                    // and never moved.
                    let mut fut = unsafe { $crate::tokio::macros::support::Pin::new_unchecked(fut) };

                    fut
                        .take_output()
                        .expect("expected completed future")
                        .ok()
                        .expect("expected Ok(_)")
                },)*)))
            }
        }).await
    }};

    // ===== Normalize =====

    (@ { rotator_select=$rotator_select:ty;  ( $($s:tt)* ) ( $($n:tt)* ) $($t:tt)* } $e:expr, $($r:tt)* ) => {
      $crate::__veil_tokio_try_join!(@{ rotator_select=$rotator_select; ($($s)* _) ($($n)* + 1) $($t)* ($($s)*) $e, } $($r)*)
    };

    // ===== Entry point =====
    ( biased; $($e:expr),+ $(,)?) => {
        $crate::__veil_tokio_try_join!(@{ rotator_select=$crate::tokio::macros::support::SelectBiased;  () (0) } $($e,)*)
    };

    ( $($e:expr),+ $(,)?) => {
        $crate::__veil_tokio_try_join!(@{ rotator_select=$crate::tokio::macros::support::SelectNormal; () (0) } $($e,)*)
    };

    (biased;) => { async { Ok(()) }.await };

    () => { async { Ok(()) }.await }
}

/// Helper trait to select which type of `Rotator` to use.
// We need this to allow specifying a const generic without
// colliding with caller const names due to macro hygiene.
pub trait RotatorSelect {
    type Rotator<const COUNT: u32>: Default;
}

/// Marker type indicating that the starting branch should
/// rotate each poll.
#[derive(Debug)]
pub struct SelectNormal;
/// Marker type indicating that the starting branch should
/// be the first declared branch each poll.
#[derive(Debug)]
pub struct SelectBiased;

impl RotatorSelect for SelectNormal {
    type Rotator<const COUNT: u32> = Rotator<COUNT>;
}

impl RotatorSelect for SelectBiased {
    type Rotator<const COUNT: u32> = BiasedRotator;
}

/// Rotates by one each [`Self::num_skip`] call up to COUNT - 1.
#[derive(Default, Debug)]
pub struct Rotator<const COUNT: u32> {
    next: u32,
}

impl<const COUNT: u32> Rotator<COUNT> {
    /// Rotates by one each [`Self::num_skip`] call up to COUNT - 1
    #[inline]
    pub fn num_skip(&mut self) -> u32 {
        let num_skip = self.next;
        self.next += 1;
        if self.next == COUNT {
            self.next = 0;
        }
        num_skip
    }
}

/// [`Self::num_skip`] always returns 0.
#[derive(Default, Debug)]
pub struct BiasedRotator {}

impl BiasedRotator {
    /// Always returns 0.
    #[inline]
    pub fn num_skip(&mut self) -> u32 {
        0
    }
}
