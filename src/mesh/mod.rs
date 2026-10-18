//! サービスメッシュのデータプレーン（F-215〜F-225）。
//!
//! Istio の Envoy（pilot-agent は残してバイナリだけを差し替える。F-222）と Linkerd の linkerd2-proxy（F-223）を
//! veil で置き換える。制御プレーン（istiod・Linkerd の destination / identity / policy）は無改造で使う。
//!
//! # 構成
//!
//! - 制御プレーンに依存しない中間表現（[`model`]、F-218）をアダプタ（[`envoy`]・`linkerd`）が作り、
//!   リソース単位で `ArcSwap` に差し替える（[`state`]）。データプレーンはアダプタを知らない。
//! - データプレーンは `src/mesh/` に閉じた専用のエンジン（透過転送 [`net`]・検出 [`detect`]・mTLS [`identity`]・
//!   HTTP [`http1`]/[`h2`]・L4 [`tcp`]）。**既存のプロキシ（`proxy.rs` 等）の経路には手を入れない**
//!   （F-215 の必須条件: メッシュを使わない既存のプロキシの性能を退行させない）。HTTP/2 のサーバ側の
//!   フレーム処理だけは `proxy.rs` の駆動部を総称化して共用する（既存の呼び出しは単相化で同じコード）。
//! - 制御プレーンとの通信（xDS・SDS・Identity）は専用スレッド。データプレーンは `ArcSwap` の読み出しだけ。
//! - Linux のみ（`SO_ORIGINAL_DST`・iptables の REDIRECT が前提）。

#[cfg(not(target_os = "linux"))]
compile_error!("the `mesh` feature is Linux-only (SO_ORIGINAL_DST / iptables REDIRECT)");

pub mod accesslog;
pub mod admin;
pub mod detect;
pub mod envoy;
pub mod h2;
pub mod hbone;
pub mod http1;
pub mod httpcore;
pub mod identity;
pub mod jwt;
pub mod l7;
pub mod mirror;
pub mod model;
pub mod net;
pub use crate::path_norm;
pub mod peer;
pub mod server;
pub mod state;
pub mod stats;
pub mod tcp;
pub mod tracing;
pub mod upstream;
#[cfg(feature = "wasm")]
pub mod wasm;
pub mod xds;

#[cfg(feature = "mesh-linkerd")]
pub mod linkerd;

/// veil が Envoy として起動されたか（argv[0] のファイル名が `envoy`、または第 1 引数が `envoy`）。
///
/// Istio の pilot-agent は `ProxyConfig.binaryPath`（既定 `/usr/local/bin/envoy`）を起動するので、
/// メッシュ用イメージでは veil をその位置に置く（または `envoy` という名前のリンクにする）。
pub fn envoy_invocation() -> Option<Vec<String>> {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().map(|a| a.as_str()).unwrap_or("");
    let base = std::path::Path::new(argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if base == "envoy" {
        return Some(args.into_iter().skip(1).collect());
    }
    if args.get(1).map(|a| a.as_str()) == Some("envoy") {
        return Some(args.into_iter().skip(2).collect());
    }
    None
}

/// Linkerd のプロキシとして起動されたか（argv[0] が `linkerd2-proxy`、第 1 引数が `linkerd-proxy`、
/// または環境変数 `LINKERD2_PROXY_IDENTITY_DIR` 等の proxy-injector が渡す設定がある）。
#[cfg(feature = "mesh-linkerd")]
pub fn linkerd_invocation() -> bool {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let base = std::path::Path::new(&argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    base == "linkerd2-proxy"
        || args.next().as_deref() == Some("linkerd-proxy")
        || std::env::var_os("LINKERD2_PROXY_INBOUND_LISTEN_ADDR").is_some()
}
