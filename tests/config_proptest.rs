//! F-56: 設定パーサ（`veil::config`）のプロパティベーステスト
//!
//! 設定はプロキシの信頼境界の外から与えられ得る文字列（プロキシ URL・TOML 本文）を
//! パースする経路である。ここでは proptest で任意入力を大量生成し、次の **不変条件** を
//! 検証する（テストを通すためだけの空テストではなく、実際に panic・非決定性・
//! パース/検証の不整合を検出する）。
//!
//! 1. `ProxyTarget::parse` は任意文字列で panic せず、`Some`/`None` を **決定的**に返す。
//! 2. well-formed な `scheme://host[:port][/path]` は必ず `Some` になり、
//!    host / port / use_tls / path_prefix が入力どおりに復元される（ラウンドトリップ）。
//! 3. スキーム欠落（`http(s)://` で始まらない）入力は必ず `None`。
//! 4. 明示ポートなしの既定ポート意味論（http=80 / https=443）と `is_default_port` の整合。
//! 5. `test_config_file` は任意バイト列でも panic せず、必ず `Ok`/`Err` を返す（決定的）。
//!    TOML として妥当でも cert/key 不在等で検証エラーになるが、**クラッシュしない**こと。

use proptest::prelude::*;
use std::io::Write;
use veil::config::ProxyTarget;

/// スキーム・コロン・スラッシュを含まない安全なホストラベル。
fn host_label() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9][a-zA-Z0-9.-]{0,30}"
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// 不変条件 1: 任意文字列で panic せず、同一入力で結果が決定的。
    #[test]
    fn proxy_target_parse_never_panics_and_is_deterministic(s in ".{0,80}") {
        let a = ProxyTarget::parse(&s);
        let b = ProxyTarget::parse(&s);
        prop_assert_eq!(a.is_some(), b.is_some(), "parse must be deterministic for {:?}", s);
        if let (Some(a), Some(b)) = (a, b) {
            prop_assert_eq!(a.host, b.host);
            prop_assert_eq!(a.port, b.port);
            prop_assert_eq!(a.use_tls, b.use_tls);
            prop_assert_eq!(a.path_prefix, b.path_prefix);
        }
    }

    /// 不変条件 2 + 4: well-formed URL のラウンドトリップと既定ポート意味論。
    #[test]
    fn proxy_target_roundtrips_wellformed_url(
        https in any::<bool>(),
        host in host_label(),
        explicit_port in prop::option::of(any::<u16>()),
        path in prop::option::of("/[a-zA-Z0-9/_-]{0,20}"),
    ) {
        let scheme = if https { "https" } else { "http" };
        let port_part = explicit_port.map(|p| format!(":{p}")).unwrap_or_default();
        let path_part = path.clone().unwrap_or_default();
        let url = format!("{scheme}://{host}{port_part}{path_part}");

        let t = ProxyTarget::parse(&url)
            .unwrap_or_else(|| panic!("well-formed URL must parse: {url}"));

        prop_assert_eq!(&t.host, &host, "host must round-trip for {}", url);
        prop_assert_eq!(t.use_tls, https, "scheme must map to use_tls for {}", url);

        // ポート: 明示ポートはそのまま、省略時は既定（https=443 / http=80）。
        let expected_port = explicit_port.unwrap_or(if https { 443 } else { 80 });
        prop_assert_eq!(t.port, expected_port, "port mismatch for {}", url);

        // 明示ポートなしのとき is_default_port は必ず true。
        if explicit_port.is_none() {
            prop_assert!(t.is_default_port(), "no explicit port ⇒ default port for {}", url);
        }

        // パス: 省略時は "/"、指定時はそのまま前置される。
        let expected_path = path.unwrap_or_else(|| "/".to_string());
        prop_assert_eq!(&t.path_prefix, &expected_path, "path_prefix mismatch for {}", url);
    }

    /// 不変条件 3: スキーム欠落は必ず None。
    #[test]
    fn proxy_target_rejects_schemeless(s in "[^:/][a-zA-Z0-9.:/_-]{0,40}") {
        // 先頭が http:// でも https:// でもないことを担保（生成戦略上ほぼ満たすが明示チェック）。
        prop_assume!(!s.starts_with("http://") && !s.starts_with("https://"));
        prop_assert!(
            ProxyTarget::parse(&s).is_none(),
            "schemeless input must be rejected: {:?}", s
        );
    }

    /// 不変条件 5: 任意 TOML 本文でも `test_config_file` は panic せず Ok/Err を返す（決定的）。
    ///
    /// 構造化された `[server]`/`[tls]` を含む「それらしい」設定と、無構造なゴミの両方を
    /// カバーする。cert/key 不在で Err になるのは正常（クラッシュしないことを検査）。
    #[test]
    fn test_config_file_never_panics(
        listen in "[0-9.]{0,20}:[0-9]{0,6}",
        junk in ".{0,60}",
        structured in any::<bool>(),
    ) {
        let body = if structured {
            format!(
                "[server]\nlisten = \"{listen}\"\n\n[tls]\ncert_path = \"/nonexistent/cert.pem\"\nkey_path = \"/nonexistent/key.pem\"\n\n{junk}\n"
            )
        } else {
            junk.clone()
        };

        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        tmp.write_all(body.as_bytes()).expect("write");
        tmp.flush().expect("flush");

        // 2 回呼んで決定性も確認（panic しないこと自体が主目的）。
        let r1 = veil::config::test_config_file(tmp.path()).is_ok();
        let r2 = veil::config::test_config_file(tmp.path()).is_ok();
        prop_assert_eq!(r1, r2, "test_config_file must be deterministic for body {:?}", body);
    }
}
