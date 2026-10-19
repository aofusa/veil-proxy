//! ログの設定
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// ログ設定
// ====================
//
// ftlogは内部でバックグラウンドスレッドとチャネルを使用した
// 非同期ログライブラリです。以下の設定で最適化が可能です。
//
// ## grokの指摘に対する検証結果
//
// grokは「ftlogは同期ログ」と主張していましたが、これは不正確です。
// ftlogは以下の非同期アーキテクチャを使用しています：
// - ログマクロ → 内部チャネルにプッシュ（ノンブロッキング）
// - バックグラウンドスレッド → チャネルから読み取りファイルI/O
//
// したがって、tokio::sync::mpscを使った追加の非同期化層は不要であり、
// むしろオーバーヘッドを増やす可能性があります。
//
// ## 推奨される最適化
// - channel_size: 高負荷時のバックプレッシャーを軽減
// - flush_interval_ms: ディスクI/O頻度の調整
// - level: 本番環境ではinfo以上を推奨
// ====================

// LogFormat, LoggingConfigSection, parse_log_level は
// crate::logging モジュールに移動しました。

/// SO_REUSEPORTの振り分け方式
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub enum ReuseportBalancing {
    /// カーネルデフォルト（3元タプルハッシュ: protocol + source IP + source port）
    #[default]
    Kernel,
    /// フローハッシュ（skb->hash、4タプル: 送信元/宛先IP・ポート）ベースのCBPF振り分け
    /// 同一TCP接続（同一フロー）からの接続を常に同じワーカースレッドに振り分け、
    /// CPUキャッシュ効率とセッション再開効率を向上させる。
    /// skb->hash が未計算（0）の場合は受信CPU番号にフォールバックする（B-76）。
    Cbpf,
}

impl<'de> serde::Deserialize<'de> for ReuseportBalancing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "kernel" => Ok(ReuseportBalancing::Kernel),
            "cbpf" => Ok(ReuseportBalancing::Cbpf),
            other => Err(serde::de::Error::custom(format!(
                "unknown reuseport_balancing value: '{}', expected 'kernel' or 'cbpf'",
                other
            ))),
        }
    }
}

/// sendfile/splice チャンクサイズ調整モード
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum ChunkSizeMode {
    /// ファイルサイズに応じて動的にチャンクサイズを決定
    /// - 0-64KB: 64KB
    /// - 64KB-1MB: 256KB
    /// - 1MB超: 1MB
    #[default]
    Dynamic,
    /// 固定チャンクサイズを使用（manual_chunk_sizeで指定）
    Manual,
}

impl<'de> serde::Deserialize<'de> for ChunkSizeMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "dynamic" | "Dynamic" => Ok(ChunkSizeMode::Dynamic),
            "manual" | "Manual" => Ok(ChunkSizeMode::Manual),
            other => Err(serde::de::Error::custom(format!(
                "unknown chunk_size_mode value: '{}', expected 'dynamic' or 'manual'",
                other
            ))),
        }
    }
}

/// チャンクサイズモードのデフォルト値（動的調整）
pub(super) fn default_chunk_size_mode() -> ChunkSizeMode {
    ChunkSizeMode::Dynamic
}

/// 手動チャンクサイズのデフォルト値（1MB）
pub(super) fn default_manual_chunk_size() -> usize {
    1048576 // 1MB
}

/// ファイルサイズに応じた最適なチャンクサイズを計算
///
/// 動的調整モード時に使用されます。
pub fn calculate_optimal_chunk_size(file_size: u64) -> usize {
    match file_size {
        0..=65_536 => 65_536,          // 64KB以下: 64KB
        65_537..=1_048_576 => 262_144, // 1MB以下: 256KB
        _ => 1_048_576,                // 1MB超: 1MB
    }
}

#[derive(Clone, Debug)]
pub enum BackendConfig {
    /// 単一URLプロキシ（後方互換性のため維持）
    /// - sni_name: TLS接続時のSNI名（IP直打ち時にドメイン名を指定可能）
    /// - use_h2c: H2C (HTTP/2 over cleartext) を使用するかどうか
    ///
    /// 注意: security, compression, buffering, cache, modules は route 直下で設定
    Proxy {
        url: String,
        sni_name: Option<String>,
        use_h2c: bool,
        /// HTTPS 上流の HTTP/2（F-175、既定 `"auto"`）。
        http2: UpstreamHttp2,
        /// 上流の mTLS（F-217。未設定なら `None`）
        tls: Option<Box<crate::upstream_tls::UpstreamTlsSettings>>,
    },
    /// Upstream グループ参照（ロードバランシング用）
    /// 注意: security, compression, buffering, cache, modules は route 直下で設定
    ProxyUpstream { upstream: String, use_h2c: bool },
    /// File バックエンド設定
    /// - path: ファイルまたはディレクトリのパス
    /// - mode: "sendfile" または "memory"
    /// - index: ディレクトリアクセス時に返すファイル名（デフォルト: "index.html"）
    ///
    /// 注意: security, cache, open_file_cache, modules は route 直下で設定
    File {
        path: String,
        mode: String,
        index: Option<String>,
    },
    /// Redirect バックエンド設定
    /// - redirect_url: リダイレクト先URL（$request_uri, $host, $path 変数使用可能）
    /// - redirect_status: ステータスコード（301, 302, 307, 308）
    /// - preserve_path: 元のパスをリダイレクト先に追加するか
    ///
    /// 注意: modules は route 直下で設定
    Redirect {
        redirect_url: String,
        redirect_status: u16,
        preserve_path: bool,
    },
    /// ネイティブハンドラ（API ライブラリ、F-188）。`handler` は登録済みハンドラの名前。
    /// `handler` feature 無効のビルドでは `type = "Handler"` は設定エラーになる。
    #[cfg(feature = "handler")]
    Handler { handler: String },
    /// 固定応答（F-219、Envoy の `direct_response` 相当）。`status`・`body`・`content_type`
    Respond {
        status: u16,
        body: String,
        content_type: Option<String>,
    },
}

impl<'de> serde::Deserialize<'de> for BackendConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};

        struct BackendConfigVisitor;

        impl<'de> Visitor<'de> for BackendConfigVisitor {
            type Value = BackendConfig;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a backend configuration object")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut backend_type: Option<String> = None;
                let mut url: Option<String> = None;
                let mut upstream: Option<String> = None;
                let mut path: Option<String> = None;
                let mut mode: Option<String> = None;
                let mut index: Option<String> = None;
                // Redirect 用フィールド
                let mut redirect_url: Option<String> = None;
                let mut redirect_status: Option<u16> = None;
                let mut preserve_path: Option<bool> = None;
                // SNI 用フィールド（Proxy用）
                let mut sni_name: Option<String> = None;
                // H2C 用フィールド（Proxy用）
                let mut use_h2c: Option<bool> = None;
                // HTTPS 上流の HTTP/2（F-175、Proxy用）
                let mut http2: Option<UpstreamHttp2> = None;
                // ネイティブハンドラ名（Handler 用、F-188）
                let mut handler: Option<String> = None;
                // 上流の mTLS（Proxy 用、F-217）
                let mut tls = crate::upstream_tls::UpstreamTlsSettings::default();
                // 固定応答（Respond 用、F-219）
                let mut status: Option<u16> = None;
                let mut body: Option<String> = None;
                let mut content_type: Option<String> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => backend_type = Some(map.next_value()?),
                        "url" => url = Some(map.next_value()?),
                        "upstream" => upstream = Some(map.next_value()?),
                        "path" => path = Some(map.next_value()?),
                        "mode" => mode = Some(map.next_value()?),
                        "index" => index = Some(map.next_value()?),
                        // security, compression, buffering, cache, open_file_cache, modules は route 直下で設定されるため、ここでは無視
                        "security" | "compression" | "buffering" | "cache" | "open_file_cache"
                        | "modules" => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                        "redirect_url" => redirect_url = Some(map.next_value()?),
                        "redirect_status" => redirect_status = Some(map.next_value()?),
                        "preserve_path" => preserve_path = Some(map.next_value()?),
                        "sni_name" => sni_name = Some(map.next_value()?),
                        "use_h2c" | "h2c" => use_h2c = Some(map.next_value()?),
                        "http2" => http2 = Some(map.next_value()?),
                        "handler" => handler = Some(map.next_value()?),
                        "tls_client_cert" => tls.tls_client_cert = map.next_value()?,
                        "tls_client_key" => tls.tls_client_key = map.next_value()?,
                        "tls_ca" => tls.tls_ca = map.next_value()?,
                        "expected_san" => tls.expected_san = map.next_value()?,
                        "status" => status = Some(map.next_value()?),
                        "body" => body = Some(map.next_value()?),
                        "content_type" => content_type = Some(map.next_value()?),
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let backend_type = backend_type.unwrap_or_else(|| "File".to_string());

                match backend_type.as_str() {
                    #[cfg(feature = "handler")]
                    "Handler" => {
                        let handler =
                            handler.ok_or_else(|| serde::de::Error::missing_field("handler"))?;
                        Ok(BackendConfig::Handler { handler })
                    }
                    #[cfg(not(feature = "handler"))]
                    "Handler" => {
                        let _ = handler;
                        Err(serde::de::Error::custom(
                            "action type \"Handler\" requires a build with the `handler` feature \
                             (veil API library)",
                        ))
                    }
                    "Proxy" => {
                        // upstream が指定されている場合はロードバランシング用
                        if let Some(upstream_name) = upstream {
                            let use_h2c = use_h2c.unwrap_or(false);
                            Ok(BackendConfig::ProxyUpstream {
                                upstream: upstream_name,
                                use_h2c,
                            })
                        } else {
                            let url = url.ok_or_else(|| {
                                serde::de::Error::missing_field("url or upstream")
                            })?;
                            let use_h2c = use_h2c.unwrap_or(false);
                            Ok(BackendConfig::Proxy {
                                url,
                                sni_name,
                                use_h2c,
                                http2: http2.unwrap_or_default(),
                                tls: tls.is_set().then(|| Box::new(tls)),
                            })
                        }
                    }
                    "Respond" => {
                        let status = status.unwrap_or(200);
                        if !(100..=599).contains(&status) {
                            return Err(serde::de::Error::custom(format!(
                                "invalid status: {}",
                                status
                            )));
                        }
                        Ok(BackendConfig::Respond {
                            status,
                            body: body.unwrap_or_default(),
                            content_type,
                        })
                    }
                    "Redirect" => {
                        let redirect_url = redirect_url
                            .ok_or_else(|| serde::de::Error::missing_field("redirect_url"))?;
                        let redirect_status = redirect_status.unwrap_or(301);
                        // ステータスコードの検証（301, 302, 303, 307, 308のみ許可）
                        if !matches!(redirect_status, 301 | 302 | 303 | 307 | 308) {
                            return Err(serde::de::Error::custom(format!(
                                "invalid redirect_status: {}, expected 301, 302, 303, 307, or 308",
                                redirect_status
                            )));
                        }
                        let preserve_path = preserve_path.unwrap_or(false);
                        Ok(BackendConfig::Redirect {
                            redirect_url,
                            redirect_status,
                            preserve_path,
                        })
                    }
                    _ => {
                        let path = path.ok_or_else(|| serde::de::Error::missing_field("path"))?;
                        let mode = mode.unwrap_or_else(|| "sendfile".to_string());
                        Ok(BackendConfig::File { path, mode, index })
                    }
                }
            }
        }

        deserializer.deserialize_map(BackendConfigVisitor)
    }
}
