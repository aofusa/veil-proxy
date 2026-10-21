// veil: BoringSSL の FFI の宣言を、aws-lc-sys の Rust のバインディング経由の呼び出しに切り替える（F-243）。
//
// quiche（upstream）は BoringSSL の関数を自分の `extern "C" { .. }` で宣言し、リンク時にその名前のシンボルを
// 要求する。veil は rustls と同じ aws-lc-sys を使うので、以前は aws-lc-sys を接頭辞無し（`AWS_LC_SYS_NO_PREFIX=1`）で
// ビルドして名前を合わせていた。その値は環境変数（`.cargo/config.toml` の `[env]`）でしか渡せず、veil を依存として
// 使う側にも同じ設定を要求していた。
//
// aws-lc-sys の bindgen は、関数名から接頭辞を外し、`#[link_name = "aws_lc_<版>_<名前>"]` を付けて出す
// （接頭辞無しのビルドでは付けない）。そこで `aws-lc-sys` feature では、各宣言を同じ名前・同じシグネチャの
// `#[inline(always)] unsafe fn` にし、中で `aws_lc_sys::<名前>` を呼ぶ。接頭辞の有無は aws-lc-sys が吸収するので、
// 利用者は何も設定しなくてよい。呼び出し側のコードは変えない。
//
// 型の違いは `Conv` で埋める:
// - 不透明な型へのポインタ（quiche の `SSL` と aws-lc-sys の `ssl_st` など）は `cast`（同じ C の構造体）
// - コールバック（`Option<extern "C" fn ..>`）は大きさを確かめて `transmute_copy`（ABI は C の宣言で同じ）
// - `crypto::Level`（`#[repr(C)]` の列挙）と `ssl_early_data_reason_t`（`c_uint` の新しい型）は `c_uint` と相互に
// - 同じ型の整数はそのまま
//
// feature が無いときは今までと同じ `extern "C"` の宣言を出す（非 Linux の `boringssl-boring-crate` など）。

/// `extern "C"` の宣言の並びを受け取り、`aws-lc-sys` feature の有無で宣言か包みの関数を出す。
macro_rules! veil_ffi {
    ($(
        $(#[$m:meta])*
        $v:vis fn $name:ident($($a:ident: $t:ty),* $(,)?) $(-> $r:ty)?;
    )*) => {
        #[cfg(not(feature = "aws-lc-sys"))]
        extern "C" {
            $(
                $(#[$m])*
                $v fn $name($($a: $t),*) $(-> $r)?;
            )*
        }
        $(
            #[cfg(feature = "aws-lc-sys")]
            $(#[$m])*
            #[inline(always)]
            #[allow(non_snake_case, clippy::too_many_arguments)]
            $v unsafe fn $name($($a: $t),*) $(-> $r)? {
                $crate::veil_ffi::Conv::conv($crate::veil_ffi::sys::$name(
                    $($crate::veil_ffi::Conv::conv($a)),*
                ))
            }
        )*
    };
}

#[cfg(feature = "aws-lc-sys")]
pub(crate) use conv::Conv;

#[cfg(feature = "aws-lc-sys")]
mod conv {
    use std::os::raw::c_uint;

    /// quiche の宣言の型から aws-lc-sys の型へ（とその逆）の変換。
    pub(crate) trait Conv<T> {
        fn conv(self) -> T;
    }

    impl<A, B> Conv<*mut B> for *mut A {
        #[inline(always)]
        fn conv(self) -> *mut B {
            self.cast()
        }
    }

    impl<A, B> Conv<*const B> for *const A {
        #[inline(always)]
        fn conv(self) -> *const B {
            self.cast()
        }
    }

    impl<A, B> Conv<*const B> for *mut A {
        #[inline(always)]
        fn conv(self) -> *const B {
            self.cast_const().cast()
        }
    }

    impl<A, B> Conv<*mut B> for *const A {
        #[inline(always)]
        fn conv(self) -> *mut B {
            self.cast_mut().cast()
        }
    }

    // コールバック: どちらも `Option<unsafe extern "C" fn(..)>`（null か関数のアドレス）。引数の型の名前が違うだけで
    // C の宣言は同じなので、そのまま読み替える。
    impl<F: Copy, G: Copy> Conv<Option<G>> for Option<F> {
        #[inline(always)]
        fn conv(self) -> Option<G> {
            const {
                assert!(std::mem::size_of::<Option<F>>() == std::mem::size_of::<Option<G>>());
            }
            // SAFETY: どちらも関数ポインタの Option で、大きさは上で確かめた。呼び出し規約と引数の並びは
            // BoringSSL / aws-lc の同じ C の宣言に従う（quiche の宣言と aws-lc-sys の bindgen の出力）。
            unsafe { std::mem::transmute_copy(&self) }
        }
    }

    // quiche が戻り値を宣言していない関数（`ERR_error_string_n` は aws-lc では書いたバッファを返す）。
    impl<A> Conv<()> for *mut A {
        #[inline(always)]
        fn conv(self) {}
    }

    // quiche が `*const c_void`（null を渡すだけ）で宣言しているコールバックの引数
    // （`SSL_get_ex_new_index` の dup_unused・free_func）。
    impl<G: Copy> Conv<Option<G>> for *const std::os::raw::c_void {
        #[inline(always)]
        fn conv(self) -> Option<G> {
            const {
                assert!(std::mem::size_of::<Option<G>>() == std::mem::size_of::<*const std::os::raw::c_void>());
            }
            // SAFETY: 関数ポインタの Option はポインタと同じ大きさで、null は None になる。
            unsafe { std::mem::transmute_copy(&self) }
        }
    }

    impl Conv<c_uint> for crate::crypto::Level {
        #[inline(always)]
        fn conv(self) -> c_uint {
            self as c_uint
        }
    }

    impl Conv<crate::crypto::Level> for c_uint {
        #[inline(always)]
        fn conv(self) -> crate::crypto::Level {
            // SAFETY: `ssl_encryption_level_t` は 0..=3 で、`crypto::Level`（`#[repr(C)]`）と同じ値・同じ大きさ。
            unsafe { std::mem::transmute::<c_uint, crate::crypto::Level>(self) }
        }
    }

    // `ssl_early_data_reason_t`（tls/boringssl.rs の非公開の型）への変換は、その型の隣に置いている。

    macro_rules! same {
        ($($t:ty),*) => {$(
            impl Conv<$t> for $t {
                #[inline(always)]
                fn conv(self) -> $t {
                    self
                }
            }
        )*};
    }
    same!((), i8, u8, i16, u16, i32, u32, i64, u64, isize, usize);
}

/// aws-lc-sys の関数。名前が違うものだけここで合わせる（ここの定義が glob の import より優先される）。
#[cfg(feature = "aws-lc-sys")]
pub(crate) mod sys {
    pub(crate) use aws_lc_sys::*;

    /// BoringSSL の `sk_num`（aws-lc のバインディングは `OPENSSL_sk_num`）。
    #[inline(always)]
    pub(crate) unsafe fn sk_num(sk: *const aws_lc_sys::OPENSSL_STACK) -> usize {
        aws_lc_sys::OPENSSL_sk_num(sk)
    }

    /// BoringSSL の `sk_value`（aws-lc のバインディングは `OPENSSL_sk_value`）。
    #[inline(always)]
    pub(crate) unsafe fn sk_value(sk: *const aws_lc_sys::OPENSSL_STACK, i: usize) -> *mut std::os::raw::c_void {
        aws_lc_sys::OPENSSL_sk_value(sk, i)
    }
}
