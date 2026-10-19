//! 圧縮の設定（プロキシのバックエンド用）
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

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
    /// Accept-Encodingヘッダーから最適な圧縮方式を選択
    ///
    /// 優先順位: zstd > br > gzip > deflate > identity
    /// q値（品質値）も考慮します。
    pub fn parse(value: &[u8]) -> Self {
        let value_str = match std::str::from_utf8(value) {
            Ok(s) => s.to_ascii_lowercase(),
            Err(_) => return Self::Identity,
        };

        // q値を考慮した解析
        let mut best = (Self::Identity, 0.0f32);

        for part in value_str.split(',') {
            let part = part.trim();
            let (encoding, q) = if let Some((enc, q_part)) = part.split_once(";q=") {
                (enc.trim(), q_part.trim().parse().unwrap_or(1.0))
            } else {
                (part, 1.0)
            };

            let candidate = match encoding {
                "zstd" => (Self::Zstd, q),
                "br" => (Self::Brotli, q),
                "gzip" => (Self::Gzip, q),
                "deflate" => (Self::Deflate, q),
                "*" => (Self::Gzip, q * 0.9), // * は gzip として扱う
                _ => continue,
            };

            // q値が高いもの、または同じq値ならZstd > Brotliを優先
            if candidate.1 > best.1
                || (candidate.1 == best.1 && matches!(candidate.0, Self::Zstd))
                || (candidate.1 == best.1
                    && matches!(candidate.0, Self::Brotli)
                    && !matches!(best.0, Self::Zstd))
            {
                best = candidate;
            }
        }

        best.0
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

/// ルートごとの圧縮設定
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CompressionConfig {
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
        }
    }
}

impl CompressionConfig {
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
        client_encoding: AcceptedEncoding,
        content_type: Option<&[u8]>,
        content_length: Option<usize>,
        existing_encoding: Option<&[u8]>,
    ) -> Option<AcceptedEncoding> {
        // 1. 圧縮が無効
        if !self.enabled {
            return None;
        }

        // 2. クライアントが圧縮非対応
        if client_encoding == AcceptedEncoding::Identity {
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

        // スキップ対象をチェック
        if self.skip_types.iter().any(|skip| ct_str.starts_with(skip)) {
            return None;
        }

        // 圧縮対象をチェック
        if !self
            .compressible_types
            .iter()
            .any(|t| ct_str.starts_with(t))
        {
            return None;
        }

        // 5. サイズ確認（Content-Length 不明なら圧縮を試みる）
        if let Some(len) = content_length {
            if len < self.min_size {
                debug!(
                    "[Compression] Content-Length {} is below min_size {}, skipping",
                    len, self.min_size
                );
                return None;
            }
        }

        // 6. クライアントがサポートし、かつ設定で許可されている圧縮方式を選択
        let client_supports = |enc: &str| -> bool {
            matches!(
                (enc, client_encoding),
                ("zstd", AcceptedEncoding::Zstd)
                    | ("br", AcceptedEncoding::Brotli | AcceptedEncoding::Zstd)
                    | (
                        "gzip",
                        AcceptedEncoding::Gzip | AcceptedEncoding::Brotli | AcceptedEncoding::Zstd,
                    )
                    | (
                        "deflate",
                        AcceptedEncoding::Deflate
                            | AcceptedEncoding::Gzip
                            | AcceptedEncoding::Brotli
                            | AcceptedEncoding::Zstd,
                    )
            )
        };

        for enc in &self.preferred_encodings {
            if client_supports(enc) {
                return match enc.as_str() {
                    "zstd" if client_encoding == AcceptedEncoding::Zstd => {
                        Some(AcceptedEncoding::Zstd)
                    }
                    "br" if matches!(
                        client_encoding,
                        AcceptedEncoding::Brotli | AcceptedEncoding::Zstd
                    ) =>
                    {
                        Some(AcceptedEncoding::Brotli)
                    }
                    "gzip"
                        if matches!(
                            client_encoding,
                            AcceptedEncoding::Gzip
                                | AcceptedEncoding::Brotli
                                | AcceptedEncoding::Zstd
                        ) =>
                    {
                        Some(AcceptedEncoding::Gzip)
                    }
                    "deflate" => Some(AcceptedEncoding::Deflate),
                    _ => continue,
                };
            }
        }

        // クライアントの圧縮方式を使用
        Some(client_encoding)
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
    path_compression: &CompressionConfig,
    http3_config: &Http3ConfigSection,
) -> CompressionConfig {
    // パスごとの設定で明示的に有効化されている場合はそれを優先
    // （パス設定が既に有効なら、HTTP/3設定で上書きするだけ）
    if path_compression.enabled {
        // パス設定が有効な場合、HTTP/3専用パラメータで上書き
        let h3_comp = &http3_config.compression;
        return CompressionConfig {
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
        };
    }

    // パスごとの設定で圧縮が無効の場合
    // HTTP/3の compression_enabled をチェック
    if http3_config.compression_enabled {
        // HTTP/3で圧縮が有効化されている場合、HTTP/3専用設定を適用
        let h3_comp = &http3_config.compression;
        return CompressionConfig {
            enabled: true, // HTTP/3では有効
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
        };
    }

    // HTTP/3圧縮も無効の場合はパス設定をそのまま使用（圧縮無効）
    path_compression.clone()
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
