//! 圧縮の設定（プロキシのバックエンド用）
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;
use std::sync::Arc;

// ====================
// 圧縮設定（プロキシバックエンド用）
// ====================
//
// ルートごとにレスポンス圧縮を設定できます。
// デフォルトは無効で、kTLS最適化を維持します。
//
// 有効にすると、バックエンドからのレスポンスを動的に圧縮し、
// クライアントへ転送します。この場合、kTLSのゼロコピー最適化は
// 迂回されます。
// ====================

/// クライアントがサポートする圧縮方式
// F-169: 圧縮結果キャッシュ（`cache::compressed`）のキーの一部として使うため Hash を追加。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AcceptedEncoding {
    /// Zstandard (zstd) - 最高効率
    Zstd,
    /// Brotli (br) - 高圧縮率
    Brotli,
    /// Gzip - 標準的な圧縮
    Gzip,
    /// Deflate（互換性のため）
    Deflate,
    /// 圧縮なし
    Identity,
}

impl AcceptedEncoding {
    /// 設定の方式名（`preferred_encodings` の値）から
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "zstd" => Some(Self::Zstd),
            "br" => Some(Self::Brotli),
            "gzip" => Some(Self::Gzip),
            "deflate" => Some(Self::Deflate),
            _ => None,
        }
    }

    /// Content-Encodingヘッダー値を返す
    pub fn as_header_value(&self) -> &'static [u8] {
        match self {
            Self::Zstd => b"zstd",
            Self::Brotli => b"br",
            Self::Gzip => b"gzip",
            Self::Deflate => b"deflate",
            Self::Identity => b"identity",
        }
    }
}

/// クライアントの `Accept-Encoding` が受け付ける方式と q 値（B-302）。
///
/// 要求ごとに確保しない（以前は値を小文字にした `String` を作っていた）。q は千分率（RFC 9110 §12.4.2）で、
/// 0 は受け付けない。明示の無い方式は `*` の q に従う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct ClientAccept {
    /// zstd・br・gzip・deflate の順
    q: [u16; 4],
}

impl ClientAccept {
    /// 圧縮を受け付けない（`Accept-Encoding` が無い要求）
    pub const NONE: Self = Self { q: [0; 4] };
    /// すべての方式を受け付ける（「この応答は圧縮し得るか」の判定用）
    pub const ALL: Self = Self { q: [1000; 4] };

    fn slot(enc: AcceptedEncoding) -> Option<usize> {
        match enc {
            AcceptedEncoding::Zstd => Some(0),
            AcceptedEncoding::Brotli => Some(1),
            AcceptedEncoding::Gzip => Some(2),
            AcceptedEncoding::Deflate => Some(3),
            AcceptedEncoding::Identity => None,
        }
    }

    /// `Accept-Encoding` の値を解析する（大文字小文字・`;` の前後の空白・`*`・`q=0` を扱う）
    pub fn parse(value: &[u8]) -> Self {
        let mut explicit = [None::<u16>; 4];
        let mut star = None;
        for item in value.split(|&b| b == b',') {
            let mut parts = item.split(|&b| b == b';');
            let name = trim_ascii(parts.next().unwrap_or(b""));
            let mut q = 1000u16;
            for p in parts {
                if let Some(eq) = p.iter().position(|&b| b == b'=') {
                    if trim_ascii(&p[..eq]).eq_ignore_ascii_case(b"q") {
                        q = parse_q(trim_ascii(&p[eq + 1..]));
                    }
                }
            }
            let idx = if name.eq_ignore_ascii_case(b"zstd") {
                0
            } else if name.eq_ignore_ascii_case(b"br") {
                1
            } else if name.eq_ignore_ascii_case(b"gzip") || name.eq_ignore_ascii_case(b"x-gzip") {
                2
            } else if name.eq_ignore_ascii_case(b"deflate") {
                3
            } else {
                if name == b"*" {
                    star = Some(q);
                }
                continue;
            };
            explicit[idx] = Some(q);
        }
        let mut q = [0u16; 4];
        for (i, e) in explicit.iter().enumerate() {
            q[i] = e.or(star).unwrap_or(0);
        }
        Self { q }
    }

    /// `enc` の q（千分率。0 は受け付けない）
    pub fn q(&self, enc: AcceptedEncoding) -> u16 {
        Self::slot(enc).map(|i| self.q[i]).unwrap_or(0)
    }

    /// 圧縮した方式を 1 つでも受け付けるか
    pub fn accepts_any(&self) -> bool {
        self.q.iter().any(|&q| q > 0)
    }

    /// `preferred` の中でクライアントが受け付ける（q > 0）方式のうち q が最大のもの（同点は `preferred` の順）
    pub fn negotiate<S: AsRef<str>>(&self, preferred: &[S]) -> Option<AcceptedEncoding> {
        let mut best: Option<(AcceptedEncoding, u16)> = None;
        for name in preferred {
            let Some(enc) = AcceptedEncoding::from_name(name.as_ref()) else {
                continue;
            };
            let q = self.q(enc);
            if q > 0 && best.is_none_or(|(_, b)| q > b) {
                best = Some((enc, q));
            }
        }
        best.map(|(e, _)| e)
    }
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn trim_ascii(s: &[u8]) -> &[u8] {
    s.trim_ascii()
}

/// q 値を千分率に（小数は 3 桁まで。読めない値は 0）
fn parse_q(v: &[u8]) -> u16 {
    match v {
        [b'1'] => 1000,
        [b'1', b'.', rest @ ..] if rest.len() <= 3 && rest.iter().all(|&b| b == b'0') => 1000,
        [b'0'] => 0,
        [b'0', b'.', frac @ ..] if frac.len() <= 3 && frac.iter().all(|b| b.is_ascii_digit()) => {
            let mut q = 0u16;
            for i in 0..3 {
                q = q * 10 + frac.get(i).map(|d| (d - b'0') as u16).unwrap_or(0);
            }
            q
        }
        _ => 0,
    }
}

/// ルートごとの圧縮設定
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CompressionConfig {
    /// HTTP/3 で使う設定（`resolve_http3_compression_config` が初回に作って持つ。B-303）。要求ごとに
    /// 文字列の配列を複製しない。複製した設定には引き継がない（複製後に変えられても古い値を返さない）
    #[serde(skip)]
    pub(crate) h3_resolved: H3Resolved,
    /// 圧縮を有効にするかどうか
    /// デフォルト: false（kTLS最適化を維持）
    pub enabled: bool,

    /// 圧縮方式の優先順位
    /// サポート: "zstd", "br" (Brotli), "gzip", "deflate"
    /// デフォルト: ["zstd", "br", "gzip"]
    pub preferred_encodings: Vec<String>,

    /// Gzip圧縮レベル (1-9)
    /// 1: 最速（圧縮率低）、9: 最遅（圧縮率高）
    /// デフォルト: 4（バランス重視）
    #[serde(default = "default_gzip_level")]
    pub gzip_level: u32,

    /// Brotli圧縮レベル (0-11)
    /// 0: 最速、11: 最遅（圧縮率最高）
    /// デフォルト: 4（バランス重視）
    #[serde(default = "default_brotli_level")]
    pub brotli_level: u32,

    /// Zstd圧縮レベル (1-22)
    /// 1: 最速、22: 最遅（圧縮率最高）
    /// デフォルト: 3（高速重視）
    #[serde(default = "default_zstd_level")]
    pub zstd_level: i32,

    /// 最小圧縮サイズ（バイト）
    /// これより小さいレスポンスは圧縮オーバーヘッドの方が大きいためスキップ
    /// デフォルト: 1024 (1KB)
    #[serde(default = "default_compression_min_size")]
    pub min_size: usize,

    /// 圧縮対象のMIMEタイプ（プレフィックスマッチ）
    /// デフォルト: ["text/", "application/json", "application/javascript", ...]
    #[serde(default = "default_compressible_types")]
    pub compressible_types: Vec<String>,

    /// 圧縮をスキップするMIMEタイプ（プレフィックスマッチ）
    /// これらにマッチするレスポンスは圧縮対象から除外
    /// デフォルト: ["image/", "video/", "audio/", ...]
    #[serde(default = "default_skip_types")]
    pub skip_types: Vec<String>,
}

// 圧縮設定のデフォルト値
pub(super) fn default_gzip_level() -> u32 {
    4
}
pub(super) fn default_brotli_level() -> u32 {
    4
}
pub(super) fn default_zstd_level() -> i32 {
    3
} // zstdは1-22、3が高速でバランス良好
pub(super) fn default_compression_min_size() -> usize {
    1024
}

pub(super) fn default_compressible_types() -> Vec<String> {
    vec![
        "text/".into(),
        "application/json".into(),
        "application/javascript".into(),
        "application/xml".into(),
        "application/xhtml+xml".into(),
        "application/rss+xml".into(),
        "application/atom+xml".into(),
        "image/svg+xml".into(),
        "application/wasm".into(),
    ]
}

pub(super) fn default_skip_types() -> Vec<String> {
    vec![
        "image/".into(),
        "video/".into(),
        "audio/".into(),
        "application/octet-stream".into(),
        "application/zip".into(),
        "application/gzip".into(),
        "application/x-gzip".into(),
        "application/x-brotli".into(),
    ]
}

pub(super) fn default_preferred_encodings() -> Vec<String> {
    vec!["zstd".into(), "br".into(), "gzip".into()]
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false, // デフォルト無効（kTLS最適化維持）
            preferred_encodings: default_preferred_encodings(),
            gzip_level: default_gzip_level(),
            brotli_level: default_brotli_level(),
            zstd_level: default_zstd_level(),
            min_size: default_compression_min_size(),
            compressible_types: default_compressible_types(),
            skip_types: default_skip_types(),
            h3_resolved: H3Resolved::default(),
        }
    }
}

/// HTTP/3 用に解決した圧縮設定の置き場（B-303）。`Clone` は空にする
#[derive(Default)]
pub struct H3Resolved(std::sync::OnceLock<Arc<CompressionConfig>>);

impl Clone for H3Resolved {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for H3Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("H3Resolved")
    }
}

impl CompressionConfig {
    /// `compression` feature の無いビルドで圧縮を有効にした設定を警告する（設定のロード時。B-303）。
    /// このビルドの `should_compress` は常に圧縮しない（以前は圧縮しない本文に `Content-Encoding` を付けたり、
    /// プロキシ応答を 502 にしていた）
    pub fn warn_if_unbuilt(&self) {
        if self.enabled && !cfg!(feature = "compression") {
            warn!(
                "compression is enabled in the configuration but this build lacks the `compression` feature; responses are sent uncompressed"
            );
        }
    }

    /// 設定の妥当性を検証
    pub fn validate(&self) -> Result<(), String> {
        if self.gzip_level < 1 || self.gzip_level > 9 {
            return Err(format!(
                "invalid gzip_level: {} (must be 1-9)",
                self.gzip_level
            ));
        }
        if self.brotli_level > 11 {
            return Err(format!(
                "invalid brotli_level: {} (must be 0-11)",
                self.brotli_level
            ));
        }
        if self.zstd_level < 1 || self.zstd_level > 22 {
            return Err(format!(
                "invalid zstd_level: {} (must be 1-22)",
                self.zstd_level
            ));
        }
        for enc in &self.preferred_encodings {
            match enc.as_str() {
                "zstd" | "gzip" | "br" | "deflate" => {}
                _ => return Err(format!("unknown encoding: {}", enc)),
            }
        }
        Ok(())
    }

    /// レスポンスを圧縮すべきか判定
    ///
    /// # Arguments
    /// * `client_encoding` - クライアントがサポートする圧縮方式
    /// * `content_type` - レスポンスのContent-Type
    /// * `content_length` - レスポンスのContent-Length（既知の場合）
    /// * `existing_encoding` - バックエンドからのContent-Encoding
    ///
    /// # Returns
    /// 圧縮すべき場合は使用する圧縮方式、それ以外はNone
    pub fn should_compress(
        &self,
        client: ClientAccept,
        content_type: Option<&[u8]>,
        content_length: Option<usize>,
        existing_encoding: Option<&[u8]>,
    ) -> Option<AcceptedEncoding> {
        // 1. 圧縮が無効（`compression` feature の無いビルドは圧縮できない。B-303）
        if !self.enabled || !cfg!(feature = "compression") {
            return None;
        }

        // 2. クライアントが圧縮非対応
        if !client.accepts_any() {
            return None;
        }

        // 3. バックエンドが既に圧縮済み
        if let Some(enc) = existing_encoding {
            if !enc.is_empty() && !enc.eq_ignore_ascii_case(b"identity") {
                return None;
            }
        }

        // 4. Content-Type確認（Content-Type が無い場合は圧縮しない）。
        // 本関数は圧縮対象の応答ごとに呼ばれるため、ログは debug に留める
        // （以前は info! で毎リクエスト 3 行出力していた）。
        let ct_str = std::str::from_utf8(content_type?).unwrap_or("");
        debug!("[Compression] Checking Content-Type: '{}'", ct_str);

        // B-281: SSE は終わらないストリームなので、溜めて圧縮すると利用者へ何も届かない
        // （設定の compressible_types に text/ があっても圧縮しない）
        if ct_str
            .get(..17)
            .is_some_and(|p| p.eq_ignore_ascii_case("text/event-stream"))
        {
            return None;
        }

        // 圧縮対象・スキップ対象は長く一致した方を優先する（B-303。既定の skip_types の "image/" より
        // compressible_types の "image/svg+xml" が勝つ）。同じ長さならスキップ
        let longest = |list: &[String]| {
            list.iter()
                .filter(|t| starts_with_ignore_case(ct_str, t))
                .map(|t| t.len())
                .max()
        };
        match (longest(&self.compressible_types), longest(&self.skip_types)) {
            (Some(c), Some(k)) if c > k => {}
            (Some(_), None) => {}
            _ => return None,
        }

        // 5. サイズ確認（Content-Length 不明なら圧縮を試みる）
        if let Some(len) = content_length {
            // B-281: 溜める上限を超える応答は圧縮せず逐次送る
            if len > crate::pool::MAX_COLLECTED_RESPONSE_BODY {
                return None;
            }
            if len < self.min_size {
                debug!(
                    "[Compression] Content-Length {} is below min_size {}, skipping",
                    len, self.min_size
                );
                return None;
            }
        }

        // 6. クライアントが受け付ける（q > 0）方式のうち、設定で許可したものから q が最大のもの（同点は設定の順）。
        // 以前は「zstd を受け付けるクライアントは br・gzip も受け付ける」とみなし、設定に無い方式も返していた（B-302）
        client.negotiate(&self.preferred_encodings)
    }
}

/// HTTP/3用の圧縮設定を解決
///
/// 優先順位:
/// 1. パスごとの設定 (compression.enabled = false なら圧縮しない)
/// 2. HTTP/3専用設定 (http3.compression_enabled + http3.compression.*)
/// 3. パスごとのデフォルト値
///
/// # 引数
/// * `path_compression` - パスごとの圧縮設定
/// * `http3_config` - HTTP/3セクションの設定
///
/// # 戻り値
/// 解決された圧縮設定
pub fn resolve_http3_compression_config(
    path_compression: &Arc<CompressionConfig>,
    http3_config: &Http3ConfigSection,
) -> Arc<CompressionConfig> {
    // パス設定も HTTP/3 の設定も圧縮しないなら、パス設定をそのまま使う（参照カウントだけ）
    if !path_compression.enabled && !http3_config.compression_enabled {
        return path_compression.clone();
    }
    // 経路の設定はロード（リロード）ごとに作り直すので、初回に解決したものを使い回せる（B-303。
    // 以前は要求ごとに約 20 個の文字列を複製していた）
    path_compression
        .h3_resolved
        .0
        .get_or_init(|| Arc::new(merge_http3_compression(path_compression, http3_config)))
        .clone()
}

/// パス設定に HTTP/3 専用の設定を重ねる（パス設定が有効、または HTTP/3 で圧縮を有効にしたとき）
fn merge_http3_compression(
    path_compression: &CompressionConfig,
    http3_config: &Http3ConfigSection,
) -> CompressionConfig {
    let h3_comp = &http3_config.compression;
    CompressionConfig {
        h3_resolved: H3Resolved::default(),
        enabled: true,
        preferred_encodings: h3_comp
            .preferred_encodings
            .clone()
            .unwrap_or_else(|| path_compression.preferred_encodings.clone()),
        gzip_level: h3_comp.gzip_level.unwrap_or(path_compression.gzip_level),
        brotli_level: h3_comp
            .brotli_level
            .unwrap_or(path_compression.brotli_level),
        zstd_level: h3_comp.zstd_level.unwrap_or(path_compression.zstd_level),
        min_size: h3_comp.min_size.unwrap_or(path_compression.min_size),
        compressible_types: h3_comp
            .compressible_types
            .clone()
            .unwrap_or_else(|| path_compression.compressible_types.clone()),
        skip_types: h3_comp
            .skip_types
            .clone()
            .unwrap_or_else(|| path_compression.skip_types.clone()),
    }
}

/// グローバルセキュリティ設定
#[derive(Deserialize, Clone, Debug, Default)]
pub struct GlobalSecurityConfig {
    /// 起動後に降格するユーザー名（非root推奨）
    #[serde(default)]
    pub drop_privileges_user: Option<String>,

    /// 起動後に降格するグループ名
    #[serde(default)]
    pub drop_privileges_group: Option<String>,

    /// グローバル同時接続上限（0 = 無制限）
    #[serde(default)]
    pub max_concurrent_connections: usize,

    /// 最前線 DDoS 防御: ブロックする IP/CIDR のリスト（F-35）。
    /// accept 直後（TLS ハンドシェイク前・ハンドラ spawn 前）に評価し、マッチした接続を
    /// 即座に切断する。既知の不正 IP に対する高コスト処理（TLS ハンドシェイク等）を回避する。
    /// 例: ["203.0.113.0/24", "198.51.100.5"]
    #[serde(default)]
    pub blocked_ips: Vec<String>,

    // ====================
    // io_uring / seccomp セキュリティ設定
    // ====================
    /// seccompフィルタを有効化（Linux専用）
    /// システムコールを制限してio_uringの悪用を防止
    #[serde(default)]
    pub enable_seccomp: bool,

    /// seccompモード
    /// - "disabled": 無効
    /// - "log": 違反をログに記録（ブロックしない）
    /// - "filter": 違反をEPERMで拒否
    /// - "strict": 違反したプロセスをSIGKILL
    #[serde(default = "default_seccomp_mode")]
    pub seccomp_mode: String,

    /// Landlockファイルシステム制限を有効化（Linux 5.13+）
    #[serde(default)]
    pub enable_landlock: bool,

    /// Landlock読み取り専用パス
    #[serde(default = "default_landlock_read_paths")]
    pub landlock_read_paths: Vec<String>,

    /// Landlock読み書きパス
    #[serde(default = "default_landlock_write_paths")]
    pub landlock_write_paths: Vec<String>,

    // ====================
    // サンドボックス設定（bubblewrap相当）
    // ====================
    //
    // Linuxのnamespace分離、bind mounts、capabilities制限を
    // プログラム起動時に適用することで、bubblewrapと同等の
    // セキュリティ分離を実現します。
    //
    // 適用順序:
    // 1. サンドボックス（namespace分離、bind mounts、capabilities）
    // 2. 権限降格（setuid/setgid）
    // 3. Landlock（ファイルシステム制限）
    // 4. seccomp（システムコール制限）
    //
    /// サンドボックスを有効化
    /// bubblewrap相当のnamespace分離、bind mounts、capabilities制限を適用
    #[serde(default)]
    pub enable_sandbox: bool,

    /// PID namespace分離
    /// サンドボックス内のプロセスは外部のプロセスを見ることができなくなります
    #[serde(default)]
    pub sandbox_unshare_pid: bool,

    /// Mount namespace分離
    /// サンドボックス内で独自のマウントポイントを持ちます
    #[serde(default = "default_sandbox_unshare_mount")]
    pub sandbox_unshare_mount: bool,

    /// UTS namespace分離
    /// サンドボックス内で独自のホスト名を持ちます
    #[serde(default = "default_sandbox_unshare_uts")]
    pub sandbox_unshare_uts: bool,

    /// IPC namespace分離
    /// サンドボックス内で独自のIPC（共有メモリ、セマフォ等）を持ちます
    #[serde(default = "default_sandbox_unshare_ipc")]
    pub sandbox_unshare_ipc: bool,

    /// User namespace分離
    /// 注: 複雑なケースがあるためデフォルトは無効
    #[serde(default)]
    pub sandbox_unshare_user: bool,

    /// Network namespace分離
    /// 警告: trueにするとネットワーク通信ができなくなります
    /// サーバーでは通常false（--share-net相当）
    #[serde(default)]
    pub sandbox_unshare_net: bool,

    /// 読み取り専用バインドマウント
    /// source:dest 形式で指定（例: "/usr:/usr"）
    #[serde(default = "default_sandbox_ro_binds")]
    pub sandbox_ro_bind_mounts: Vec<String>,

    /// 読み書きバインドマウント
    /// source:dest 形式で指定（例: "/var/log:/var/log"）
    #[serde(default)]
    pub sandbox_rw_bind_mounts: Vec<String>,

    /// tmpfsマウント先
    /// 指定されたパスにtmpfs（メモリファイルシステム）をマウント
    #[serde(default = "default_sandbox_tmpfs")]
    pub sandbox_tmpfs_mounts: Vec<String>,

    /// /proc をマウントするかどうか
    #[serde(default = "default_true")]
    pub sandbox_mount_proc: bool,

    /// /dev に最小限のデバイスノードを作成するかどうか
    #[serde(default = "default_true")]
    pub sandbox_mount_dev: bool,

    /// ドロップするケイパビリティのリスト
    /// 例: ["CAP_SYS_ADMIN", "CAP_NET_RAW"]
    #[serde(default)]
    pub sandbox_drop_capabilities: Vec<String>,

    /// 保持するケイパビリティのリスト（他は全てドロップ）
    /// drop_capabilitiesより優先されます
    /// 例: ["CAP_NET_BIND_SERVICE"]
    #[serde(default)]
    pub sandbox_keep_capabilities: Vec<String>,

    /// サンドボックス内のホスト名
    #[serde(default = "default_sandbox_hostname")]
    pub sandbox_hostname: Option<String>,

    /// PR_SET_NO_NEW_PRIVSを設定するかどうか
    #[serde(default = "default_true")]
    pub sandbox_no_new_privs: bool,

    /// セキュリティ機能の有効化に失敗した場合の動作
    /// false: 失敗時に起動を中止（デフォルト、推奨）
    /// true: 失敗時も警告を出して起動を続行（開発・デバッグ用）
    #[serde(default = "default_allow_security_failures")]
    pub allow_security_failures: bool,

    // ====================
    // FreeBSD: capsicum（F-120 Phase 4）
    // ====================
    //
    // 非対象 OS（Linux/OpenBSD）でもキー自体は受理し、未知キー拒否にはしない
    // （設計ドキュメント 4.4 節）。適用時に警告ログを出して無視する。
    /// capsicum を有効化（FreeBSD 専用）。
    ///
    /// リスナー/接続/静的ファイル fd への `cap_rights_limit(2)` を適用する。
    #[serde(default)]
    pub enable_capsicum: bool,

    /// capability mode（`cap_enter(2)`）へ降格する（FreeBSD 専用・オプトイン）。
    ///
    /// capability mode ではグローバル名前空間操作（`bind(2)`/`connect(2)`/パス指定の
    /// `open(2)` 等）が全面禁止されるため、次の制約を **すべて** 満たす構成でのみ
    /// 適用できる（満たさない場合は警告を出して適用しない）:
    /// - プロキシ/upstream・L4 リスナーが無い（`connect(2)` 不要）
    /// - h2c / HTTP/3 / metrics の追加リスナーが無い
    ///
    /// 適用は全ワーカーの listener bind 完了後（bind は capability mode で不可のため）。
    /// 注意: 現状、リクエスト時のパス指定 `open(2)` を伴う静的ファイル配信は
    /// capability mode では失敗する（キャッシュ済み応答のみ返せる）。ディレクトリ fd +
    /// `openat(2)` 相対化による完全対応はフォローアップ（backlog 参照）。
    #[serde(default)]
    pub capsicum_capability_mode: bool,

    /// 起動時に attach する jail 名（FreeBSD 専用、root 前提）。
    /// 未設定なら jail_attach は行わない。
    #[serde(default)]
    pub jail_name: Option<String>,

    // ====================
    // OpenBSD: pledge / unveil（F-120 Phase 5）
    // ====================
    //
    // 非対象 OS（Linux/FreeBSD）でもキー自体は受理し、未知キー拒否にはしない
    // （設計ドキュメント 4.4 節）。適用時に警告ログを出して無視する。
    /// unveil(2) を有効化（OpenBSD 専用）。
    ///
    /// 設定ファイル・TLS 証明書/鍵・静的ファイルルート・アクセスログ/アプリログの
    /// ディレクトリ・ディスクキャッシュディレクトリ・WASM モジュールパスを
    /// 必要権限（"r" / "rwc"）で unveil し、最後に `unveil(NULL, NULL)` でロックする。
    #[serde(default)]
    pub enable_unveil: bool,

    /// pledge(2) を有効化（OpenBSD 専用）。
    ///
    /// 全 TLS ワーカーの listener bind 完了後に最小 promise 集合へ降格する。
    #[serde(default)]
    pub enable_pledge: bool,

    // ====================
    // macOS: sandbox_init / Seatbelt（F-125）
    // ====================
    //
    // 非対象 OS（Linux/FreeBSD/OpenBSD）でもキー自体は受理し、未知キー拒否にはしない
    // （既存の capsicum/pledge と同じ方針）。適用時に警告ログを出して無視する。
    /// sandbox_init(3)（Seatbelt）を有効化（macOS 専用）。
    ///
    /// 設定から導出した静的ファイルルート・TLS 証明書/鍵・ログ/ディスクキャッシュ
    /// ディレクトリを基に SBPL プロファイルを生成し、全 TLS ワーカーの listener bind
    /// 完了後に適用する（実機検証ができないため保守的な最小プロファイル。
    /// `crate::security::macos_sandbox` 参照）。
    #[serde(default)]
    pub enable_sandbox_macos: bool,

    // ====================
    // Windows: Job Object（v0.6.0/F-125）
    // ====================
    //
    // 非対象 OS でもキー自体は受理し、未知キー拒否にはしない（capsicum/pledge/macOS
    // sandbox と同じ方針）。適用時に警告ログを出して無視する。
    /// Job Object によるプロセスリソース制限を有効化（Windows 専用、best-effort）。
    ///
    /// `CreateJobObjectW` + `SetInformationJobObject`（`JOBOBJECT_EXTENDED_LIMIT_INFORMATION`
    /// の `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`/`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 等）で
    /// プロセスを制限し `AssignProcessToJobObject` する。実機検証ができないため保守的な
    /// 最小構成（`crate::security::windows_security` 参照）。
    #[serde(default)]
    pub enable_job_object_windows: bool,

    // ====================
    // NetBSD: chroot + 特権降格 + rlimit（F-140）
    // ====================
    //
    // 非対象 OS でもキー自体は受理し、未知キー拒否にはしない（capsicum/pledge/macOS
    // sandbox と同じ方針）。適用時に警告ログを出して無視する。
    /// 起動時に `chroot(2)` する新ルートディレクトリ（NetBSD 専用、root 前提）。
    ///
    /// NetBSD には OpenBSD の pledge/unveil に相当するランタイム API が無いため、
    /// veil が提供できる範囲は `chroot(2)` + 特権降格
    /// （`drop_privileges_user`/`drop_privileges_group`）+ rlimit のみ
    /// （`crate::security::netbsd` 参照）。未設定なら chroot は行わない。
    /// chroot 後にプロセスがアクセスする全パス（静的ファイルルート・TLS 証明書/鍵・
    /// WASM モジュール・ログ/キャッシュディレクトリ）は新ルート基準で解決可能である
    /// 必要があり、これは運用側の設定責任である。
    #[serde(default)]
    pub chroot_dir: Option<String>,

    /// 要求のパスの正規化（HTTP/1.1・HTTP/2・HTTP/3。ルートの照合・`[route.security]` の検査の前に 1 回だけ行い、
    /// 正規化したパスを上流へ送る。F-236 の再走査の F7）。
    ///
    /// - `"base"`（既定）: 予約されていない文字の `%XX` を戻し、`\` を `/` にし、ドットのセグメントを取り除く
    /// - `"merge_slashes"`: `base` に加えて、連続する `/` を 1 つにする
    /// - `"off"`: 正規化しない（以前の動作。照合と上流の解釈がずれ得る）
    #[serde(default)]
    pub path_normalization: PathNormalizationMode,
}

/// `[security] path_normalization` の値
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PathNormalizationMode {
    /// 正規化しない
    Off,
    /// ドットのセグメントの除去と、予約されていない文字の復号
    #[default]
    Base,
    /// `Base` + 連続するスラッシュの統合
    MergeSlashes,
}

pub(super) fn default_allow_security_failures() -> bool {
    false // デフォルトはfalse（失敗時に起動失敗）
}

pub(super) fn default_sandbox_unshare_mount() -> bool {
    true
}
pub(super) fn default_sandbox_unshare_uts() -> bool {
    true
}
pub(super) fn default_sandbox_unshare_ipc() -> bool {
    true
}
pub(super) fn default_true() -> bool {
    true
}

pub(super) fn default_sandbox_ro_binds() -> Vec<String> {
    vec![
        "/usr:/usr".to_string(),
        "/lib:/lib".to_string(),
        "/lib64:/lib64".to_string(),
        "/etc/ssl:/etc/ssl".to_string(),
        // DNS解決に必要なファイル
        "/etc/resolv.conf:/etc/resolv.conf".to_string(),
        "/etc/hosts:/etc/hosts".to_string(),
        "/etc/nsswitch.conf:/etc/nsswitch.conf".to_string(),
        "/etc/gai.conf:/etc/gai.conf".to_string(),
        // systemd-resolved使用時に必要（存在しない場合は無視される）
        "/run/systemd/resolve:/run/systemd/resolve".to_string(),
        // ユーザー/グループ情報
        "/etc/passwd:/etc/passwd".to_string(),
        "/etc/group:/etc/group".to_string(),
    ]
}

pub(super) fn default_sandbox_tmpfs() -> Vec<String> {
    vec![
        "/tmp".to_string(),
        // 注: /run はsystemd-resolvedのDNS解決に必要なため除外
        // 必要な場合は明示的に追加してください
    ]
}

pub(super) fn default_sandbox_hostname() -> Option<String> {
    Some("veil-sandbox".to_string())
}

pub(super) fn default_seccomp_mode() -> String {
    "disabled".to_string()
}

pub(super) fn default_landlock_read_paths() -> Vec<String> {
    vec![
        "/etc".to_string(),
        "/usr".to_string(),
        "/lib".to_string(),
        "/lib64".to_string(),
    ]
}

pub(super) fn default_landlock_write_paths() -> Vec<String> {
    vec!["/var/log".to_string(), "/tmp".to_string()]
}

#[cfg(test)]
mod client_accept_tests {
    use super::*;

    const ALL: [&str; 4] = ["zstd", "br", "gzip", "deflate"];

    // B-302: q=0 は拒否・大文字小文字と空白・`*`・x-gzip・同点は設定の順
    #[test]
    fn parse_honours_q_values_case_and_wildcards() {
        assert_eq!(ClientAccept::parse(b"gzip;q=0").negotiate(&ALL), None);
        assert_eq!(
            ClientAccept::parse(b"GZIP ; Q=0.5, br;q=0").negotiate(&ALL),
            Some(AcceptedEncoding::Gzip)
        );
        assert_eq!(
            ClientAccept::parse(b"*;q=0.1, zstd;q=0").negotiate(&ALL),
            Some(AcceptedEncoding::Brotli)
        );
        assert_eq!(
            ClientAccept::parse(b"x-gzip").negotiate(&ALL),
            Some(AcceptedEncoding::Gzip)
        );
        assert_eq!(
            ClientAccept::parse(b"gzip, br").negotiate(&["gzip", "br"]),
            Some(AcceptedEncoding::Gzip)
        );
        assert_eq!(
            ClientAccept::parse(b"gzip;q=0.5, br").negotiate(&["gzip", "br"]),
            Some(AcceptedEncoding::Brotli)
        );
        assert_eq!(ClientAccept::parse(b"br;q=0").negotiate(&ALL), None);
        assert_eq!(
            ClientAccept::parse(b"gzip;q=0, *").negotiate(&["gzip", "br"]),
            Some(AcceptedEncoding::Brotli)
        );
        assert_eq!(ClientAccept::parse(b"*;q=0").negotiate(&ALL), None);
        assert!(!ClientAccept::parse(b"identity;q=0").accepts_any());
        assert!(!ClientAccept::parse(b"identity").accepts_any());
        assert!(!ClientAccept::parse(&[0xff, 0xfe]).accepts_any());
    }

    // B-302: 設定で許可していない方式は（クライアントが受け付けても）選ばない
    #[cfg(feature = "compression")]
    #[test]
    fn should_compress_only_picks_configured_encodings() {
        let cfg = CompressionConfig {
            enabled: true,
            preferred_encodings: vec!["gzip".to_string()],
            ..Default::default()
        };
        let pick = |ae: &[u8]| {
            cfg.should_compress(
                ClientAccept::parse(ae),
                Some(b"text/html"),
                Some(4096),
                None,
            )
        };
        assert_eq!(pick(b"zstd, br"), None);
        assert_eq!(pick(b"zstd, gzip"), Some(AcceptedEncoding::Gzip));
    }

    // B-303: 長く一致した方を優先する（skip_types の image/ より compressible_types の image/svg+xml）
    #[cfg(feature = "compression")]
    #[test]
    fn svg_is_compressed_despite_image_skip() {
        let cfg = CompressionConfig {
            enabled: true,
            ..Default::default()
        };
        let all = ClientAccept::ALL;
        if cfg.compressible_types.iter().any(|t| t == "image/svg+xml") {
            assert!(cfg
                .should_compress(all, Some(b"image/svg+xml"), Some(4096), None)
                .is_some());
        }
        assert!(cfg
            .should_compress(all, Some(b"image/png"), Some(4096), None)
            .is_none());
    }
}
