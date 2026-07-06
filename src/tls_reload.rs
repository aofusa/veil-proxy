//! 0 ダウンタイム TLS 証明書更新（F-03）
//!
//! 証明書・秘密鍵ファイルの更新を検知し、`ArcSwap<ServerConfig>` を差し替える。
//! 既存の接続は古い `ServerConfig`（ハンドシェイク時の snapshot）を使い続け、
//! 新しいハンドシェイクのみが新しい設定を使う。これは ArcSwap の load()
//! スナップショットを毎ハンドシェイクで取ることで自然に実現される。
//!
//! inotify などのカーネル依存は使わず、`std::fs::metadata().modified()`
//! による mtime 比較でファイル変更を検知する。
//!
//! - SIGHUP 受信時: `reload_now()` を呼ぶ（main.rs のリロードスレッド）
//! - 定期チェック（既定 60 秒）: `check_and_reload()` を呼ぶ

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use arc_swap::ArcSwap;
use rustls::ServerConfig;

/// 証明書ビルダー型
///
/// 証明書・秘密鍵パスから `Arc<ServerConfig>` を構築するクロージャ。
/// ALPN や kTLS 用のシークレット抽出など、初回ロードと同じ設定を再現する。
pub type ServerConfigBuilder =
    Box<dyn Fn(&PathBuf, &PathBuf) -> anyhow::Result<Arc<ServerConfig>> + Send + Sync>;

/// グローバルな TLS 設定スワップ
///
/// アクセプタはここから毎ハンドシェイク `load_full()` でスナップショットを
/// 取得する。`None` の場合は従来通り起動時の固定 `Arc<ServerConfig>` を使う。
pub static GLOBAL_TLS_CONFIG: once_cell::sync::Lazy<ArcSwap<Option<Arc<ServerConfig>>>> =
    once_cell::sync::Lazy::new(|| ArcSwap::from_pointee(None));

/// グローバル TLS 設定を初期化する（起動時に呼ぶ）
pub fn init_global_tls_config(config: Arc<ServerConfig>) {
    GLOBAL_TLS_CONFIG.store(Arc::new(Some(config)));
}

/// 現在のグローバル TLS 設定のスナップショットを取得する。
///
/// 未初期化の場合は None。アクセプタはハンドシェイク毎にこれを呼ぶことで、
/// リロード後の新規ハンドシェイクが新しい証明書を使用する。
#[inline]
pub fn current_global_tls_config() -> Option<Arc<ServerConfig>> {
    GLOBAL_TLS_CONFIG.load().as_ref().clone()
}

/// TLS 証明書リローダー
pub struct TlsCertReloader {
    cert_path: PathBuf,
    key_path: PathBuf,
    server_config: Arc<ArcSwap<Option<Arc<ServerConfig>>>>,
    builder: ServerConfigBuilder,
    last_modified: SystemTime,
}

impl TlsCertReloader {
    /// 新しいリローダーを作成する。
    ///
    /// `server_config` は差し替え対象の ArcSwap。`builder` は証明書から
    /// `ServerConfig` を構築するクロージャ。
    pub fn new(
        cert_path: PathBuf,
        key_path: PathBuf,
        server_config: Arc<ArcSwap<Option<Arc<ServerConfig>>>>,
        builder: ServerConfigBuilder,
    ) -> anyhow::Result<Self> {
        let last_modified = Self::combined_mtime(&cert_path, &key_path)?;
        Ok(Self {
            cert_path,
            key_path,
            server_config,
            builder,
            last_modified,
        })
    }

    /// グローバル ArcSwap を対象にしたリローダーを作成する。
    pub fn new_global(
        cert_path: PathBuf,
        key_path: PathBuf,
        builder: ServerConfigBuilder,
    ) -> anyhow::Result<Self> {
        // Lazy static の中身を Arc として共有する代わりに、グローバルへ書き戻すラッパを使う。
        // ここでは独立した ArcSwap を保持し、reload 時に GLOBAL へも反映する。
        let swap: Arc<ArcSwap<Option<Arc<ServerConfig>>>> =
            Arc::new(ArcSwap::from_pointee(current_global_tls_config()));
        Self::new(cert_path, key_path, swap, builder)
    }

    /// 証明書と秘密鍵の mtime のうち新しい方を返す。
    // 理由付き allow: 専用 TLS リロードスレッドから呼ばれる mtime 検査（イベントループ外・500ms 周期）。
    #[allow(clippy::disallowed_methods)]
    fn combined_mtime(cert: &PathBuf, key: &PathBuf) -> anyhow::Result<SystemTime> {
        let cert_m = std::fs::metadata(cert)?.modified()?;
        let key_m = std::fs::metadata(key)?.modified()?;
        Ok(cert_m.max(key_m))
    }

    /// ファイルの mtime 変化を検知し、変化していればリロードする。
    ///
    /// # Returns
    /// 実際にリロードした場合 true
    pub fn check_and_reload(&mut self) -> bool {
        let current = match Self::combined_mtime(&self.cert_path, &self.key_path) {
            Ok(m) => m,
            Err(e) => {
                ftlog::warn!("TLS cert mtime check failed: {}", e);
                return false;
            }
        };

        if current > self.last_modified {
            match self.reload_now() {
                Ok(()) => {
                    ftlog::info!("TLS certificate reloaded (mtime changed)");
                    true
                }
                Err(e) => {
                    ftlog::error!("TLS certificate reload failed: {}", e);
                    false
                }
            }
        } else {
            false
        }
    }

    /// 即座にリロードする（SIGHUP 等で呼ぶ）。
    ///
    /// 新しい `ServerConfig` を構築し、ArcSwap とグローバルへ反映する。
    /// 既存接続には影響しない（ハンドシェイク時の snapshot を使うため）。
    pub fn reload_now(&mut self) -> anyhow::Result<()> {
        let new_config = (self.builder)(&self.cert_path, &self.key_path)?;
        // ローカル ArcSwap を更新
        self.server_config.store(Arc::new(Some(new_config.clone())));
        // グローバルへも反映（アクセプタが参照）
        init_global_tls_config(new_config);
        // mtime を更新（再ロードの多重発火を防ぐ）
        self.last_modified = Self::combined_mtime(&self.cert_path, &self.key_path)?;
        Ok(())
    }

    /// 現在の最終更新時刻（テスト用）
    pub fn last_modified(&self) -> SystemTime {
        self.last_modified
    }
}

#[cfg(test)]
mod tests {
    // 理由付き allow: テストコードは同期 I/O・sleep を使用してよい（データプレーン非経由）。
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// CryptoProvider をプロセスに一度だけインストールする（テスト用）
    fn ensure_provider() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
    }

    /// テスト用にダミーの ServerConfig を作る（自己署名証明書）
    fn make_dummy_config() -> Arc<ServerConfig> {
        ensure_provider();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
        let key_der =
            rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap();
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        Arc::new(config)
    }

    /// テスト用 PEM を書き出す
    fn write_self_signed(cert_path: &PathBuf, key_path: &PathBuf) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let mut cf = std::fs::File::create(cert_path).unwrap();
        cf.write_all(cert.cert.pem().as_bytes()).unwrap();
        let mut kf = std::fs::File::create(key_path).unwrap();
        kf.write_all(cert.signing_key.serialize_pem().as_bytes())
            .unwrap();
    }

    #[test]
    fn check_and_reload_detects_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        write_self_signed(&cert_path, &key_path);

        let build_count = Arc::new(AtomicUsize::new(0));
        let bc = build_count.clone();
        let builder: ServerConfigBuilder = Box::new(move |_c, _k| {
            bc.fetch_add(1, Ordering::SeqCst);
            Ok(make_dummy_config())
        });

        let swap: Arc<ArcSwap<Option<Arc<ServerConfig>>>> = Arc::new(ArcSwap::from_pointee(None));
        let mut reloader =
            TlsCertReloader::new(cert_path.clone(), key_path.clone(), swap.clone(), builder)
                .unwrap();

        // mtime 変化なし → リロードしない
        assert!(!reloader.check_and_reload());
        assert_eq!(build_count.load(Ordering::SeqCst), 0);

        // mtime を未来に進めてファイルを再生成
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_self_signed(&cert_path, &key_path);

        // 変化検知 → リロードする
        assert!(reloader.check_and_reload());
        assert_eq!(build_count.load(Ordering::SeqCst), 1);
        assert!(swap.load().is_some());

        // 連続呼び出しでは再ロードしない（mtime 更新済み）
        assert!(!reloader.check_and_reload());
        assert_eq!(build_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reload_now_updates_global() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        write_self_signed(&cert_path, &key_path);

        let builder: ServerConfigBuilder = Box::new(move |_c, _k| Ok(make_dummy_config()));
        let swap: Arc<ArcSwap<Option<Arc<ServerConfig>>>> = Arc::new(ArcSwap::from_pointee(None));
        let mut reloader =
            TlsCertReloader::new(cert_path, key_path, swap.clone(), builder).unwrap();

        reloader.reload_now().unwrap();
        assert!(swap.load().is_some());
        assert!(current_global_tls_config().is_some());
    }
}
