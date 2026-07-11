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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

// ============================================================================
// HTTP/3 (QUIC/quiche) 証明書ホットリロード（F-105）
// ============================================================================
//
// HTTP/1.1・HTTP/2 は上の `GLOBAL_TLS_CONFIG`（rustls `ServerConfig`）で無停止更新できるが、
// HTTP/3 は各ワーカーが起動時に構築した `quiche::Config`（`Rc<RefCell<..>>`）を保持し続け、
// 差し替え経路が無かった。ここでは cert/key の生 PEM を **ペアでアトミックに** 配信し、
// 各 HTTP/3 ワーカーが自前の `quiche::Config` へ memfd 経由で反映する仕組みを提供する。
//
// 秘密鍵の平文がグローバルに滞留し続けないよう、配信時に「未適用ワーカー数」を持たせ、
// 全ワーカーが適用し終えた時点で最後のワーカーが `secure_zero` で平文を破棄する。

/// HTTP/3 証明書マテリアル（cert/key の生 PEM）。
///
/// cert と key は常にペアで整合している必要があるため 1 つの `Arc` で束ねてアトミックに配信する。
/// `generation` はワーカーの差分検知用、`pending_workers` は秘密鍵ゼロ化の同期用。
pub struct Http3CertMaterial {
    /// 配信世代（`HTTP3_CERT_GENERATION` と一致）。ワーカーはローカル世代と比較して差分検知する。
    generation: u64,
    /// PEM 証明書チェーン（memfd 経由で quiche へロード。適用完了後にゼロ化）。
    cert_pem: Mutex<Vec<u8>>,
    /// PEM 秘密鍵（memfd 経由で quiche へロード。適用完了後にゼロ化）。
    key_pem: Mutex<Vec<u8>>,
    /// この世代を未だ適用していないワーカー数。0 到達で平文をゼロ化する。
    pending_workers: AtomicUsize,
}

impl Http3CertMaterial {
    /// 配信世代を返す。
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// cert/key PEM をロックしてクロージャに渡す（quiche へのロードに使う）。
    ///
    /// ロックはこの世代を初回適用するワーカーのみが取る **コールドパス** の同期であり、
    /// ホットパス（イベントループの毎周回）には現れない。
    pub fn load_into<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&[u8], &[u8]) -> T,
    {
        let cert = self.cert_pem.lock().expect("http3 cert mutex poisoned");
        let key = self.key_pem.lock().expect("http3 key mutex poisoned");
        f(&cert, &key)
    }

    /// このワーカーが本世代を適用完了したことを通知する。
    ///
    /// 最後（0 到達）のワーカーが cert/key の平文を `secure_zero` で **即座に** 破棄する。
    /// `fetch_sub(AcqRel)` により、他ワーカーの `load_into` 読み取りはすべて
    /// 本ゼロ化に happens-before するため、並行読み取りとの競合は起きない。
    pub fn worker_applied(&self) {
        let prev = self.pending_workers.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "http3 cert pending_workers underflow");
        if prev == 1 {
            secure_zero_vec(&mut self.cert_pem.lock().expect("http3 cert mutex poisoned"));
            secure_zero_vec(&mut self.key_pem.lock().expect("http3 key mutex poisoned"));
        }
    }
}

impl Drop for Http3CertMaterial {
    /// マテリアル破棄時に平文を必ずゼロ化する（安全網）。
    ///
    /// 通常は全ワーカー適用後に `worker_applied` で即座にゼロ化されるが、次のような
    /// 適用未完了のまま破棄されるケースでも平文がヒープに残らないよう保証する:
    /// - 短時間に SIGHUP が複数回発火し、旧世代のマテリアルが全ワーカー適用前に
    ///   `GLOBAL_HTTP3_CERTS` から差し替えられて Arc 参照が尽きたとき。
    /// - HTTP/3 ワーカーが 0 台で配信自体が行われなかったとき（この経路は
    ///   `publish_http3_certs` 内で先にゼロ化するが、二重ゼロ化は無害）。
    fn drop(&mut self) {
        // 既にゼロ化済みでも volatile 書き込みが再度走るだけで無害。
        if let Ok(mut cert) = self.cert_pem.lock() {
            secure_zero_vec(&mut cert);
        }
        if let Ok(mut key) = self.key_pem.lock() {
            secure_zero_vec(&mut key);
        }
    }
}

/// HTTP/3 証明書マテリアルのグローバル配信スロット。
///
/// リロードスレッドが `publish_http3_certs` で新マテリアルを格納し、各 HTTP/3 ワーカーが
/// 世代差分を検知したときのみ `load_http3_material` で参照する。
pub static GLOBAL_HTTP3_CERTS: once_cell::sync::Lazy<ArcSwap<Option<Arc<Http3CertMaterial>>>> =
    once_cell::sync::Lazy::new(|| ArcSwap::from_pointee(None));

/// HTTP/3 証明書の配信世代（ワーカーの安価な変更検知ゲート）。
///
/// ワーカーは毎周回この u64 を `Acquire` ロードするだけ（x86 では Relaxed と同等コスト）。
/// ローカル世代と一致すれば ArcSwap には触れず、差分時のみマテリアルを取得する。
/// `Acquire` ロードは配信側の `Release` ストアと同期し、マテリアル格納の可視性を保証する。
pub static HTTP3_CERT_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 登録済み HTTP/3 ワーカー数（起動時に各ワーカーが `register_http3_worker` で加算）。
///
/// 配信時に `pending_workers` の初期値として使う。全ワーカーが適用し終えた時点で
/// 秘密鍵をゼロ化するための基準数。
pub static HTTP3_WORKER_COUNT: AtomicUsize = AtomicUsize::new(0);

/// HTTP/3 ワーカーを 1 台登録する（各ワーカーが `run_http3_server_async` 起動時に呼ぶ）。
#[inline]
pub fn register_http3_worker() {
    HTTP3_WORKER_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// 現在の HTTP/3 証明書配信世代を返す（ワーカーの変更検知ゲート）。
#[inline]
pub fn http3_cert_generation() -> u64 {
    HTTP3_CERT_GENERATION.load(Ordering::Acquire)
}

/// 現在の HTTP/3 証明書マテリアルを取得する（差分検知後に呼ぶ）。
#[inline]
pub fn load_http3_material() -> Option<Arc<Http3CertMaterial>> {
    GLOBAL_HTTP3_CERTS.load_full().as_ref().clone()
}

/// 新しい HTTP/3 証明書 PEM を全ワーカーへ配信する（リロードスレッドから呼ぶ）。
///
/// 登録ワーカーが 0 の場合（HTTP/3 無効）は配信せず、秘密鍵の平文を即座にゼロ化して破棄する。
pub fn publish_http3_certs(mut cert_pem: Vec<u8>, mut key_pem: Vec<u8>) {
    let workers = HTTP3_WORKER_COUNT.load(Ordering::Relaxed);
    if workers == 0 {
        // HTTP/3 ワーカーが居ない → 平文を配信せず即ゼロ化。
        secure_zero_vec(&mut cert_pem);
        secure_zero_vec(&mut key_pem);
        return;
    }

    let generation = HTTP3_CERT_GENERATION.load(Ordering::Relaxed).wrapping_add(1);
    let material = Arc::new(Http3CertMaterial {
        generation,
        cert_pem: Mutex::new(cert_pem),
        key_pem: Mutex::new(key_pem),
        pending_workers: AtomicUsize::new(workers),
    });

    // 先にマテリアルを格納し、その後で世代を Release ストアする。
    // ワーカーは世代を Acquire ロードするため、世代の変化を観測した時点でマテリアルの
    // 格納は必ず可視になっている（happens-before）。
    GLOBAL_HTTP3_CERTS.store(Arc::new(Some(material)));
    HTTP3_CERT_GENERATION.store(generation, Ordering::Release);
}

/// 機密バイト列を volatile 書き込み + フェンスでゼロ化する。
///
/// コンパイラのデッドストア削除を防ぐため volatile を用いる（`http3_server::secure_zero` と同方針）。
fn secure_zero_vec(data: &mut [u8]) {
    for byte in data.iter_mut() {
        // SAFETY: `data` は有効な可変スライスであり、各要素への 1 バイト volatile 書き込みは健全。
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    std::sync::atomic::fence(Ordering::SeqCst);
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
        // F-105: HTTP/3 (quiche) ワーカーへも cert/key の生 PEM を配信する。
        // 登録ワーカーが居るときのみファイルを再読込して配信（無ければ無駄な FS 読込を避ける）。
        self.reload_http3_certs()?;
        // mtime を更新（再ロードの多重発火を防ぐ）
        self.last_modified = Self::combined_mtime(&self.cert_path, &self.key_path)?;
        Ok(())
    }

    /// HTTP/3 (quiche) ワーカーへ新しい cert/key PEM を配信する（F-105）。
    ///
    /// HTTP/3 ワーカーが 1 台も登録されていない場合は何もしない。
    // 理由付き allow: 専用 TLS リロードスレッドから呼ばれる cert/key の再読込（イベントループ外・
    // 数ヶ月に 1 回のコールドパス）。生 PEM は quiche へ memfd 経由でロードするため生バイトが要る。
    #[allow(clippy::disallowed_methods)]
    fn reload_http3_certs(&self) -> anyhow::Result<()> {
        if HTTP3_WORKER_COUNT.load(Ordering::Relaxed) == 0 {
            return Ok(());
        }
        let cert_pem = std::fs::read(&self.cert_path)?;
        let key_pem = std::fs::read(&self.key_path)?;
        publish_http3_certs(cert_pem, key_pem);
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

    /// HTTP/3 グローバル状態を触るテストは直列化する（プロセス共有の static のため）。
    static HTTP3_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn http3_publish_advances_generation_and_zeroes_on_last_worker() {
        let _g = HTTP3_TEST_LOCK.lock().unwrap();
        // ワーカー 2 台を登録した状態を模擬。
        HTTP3_WORKER_COUNT.store(2, Ordering::SeqCst);

        let gen_before = HTTP3_CERT_GENERATION.load(Ordering::SeqCst);
        publish_http3_certs(b"CERTDATA".to_vec(), b"KEYDATA".to_vec());
        let gen_after = HTTP3_CERT_GENERATION.load(Ordering::SeqCst);
        assert_eq!(gen_after, gen_before.wrapping_add(1));

        let mat = load_http3_material().expect("material must be stored");
        assert_eq!(mat.generation(), gen_after);
        mat.load_into(|c, k| {
            assert_eq!(c, b"CERTDATA");
            assert_eq!(k, b"KEYDATA");
        });

        // 1 台目適用 → まだ最後ではないので平文は保持される。
        mat.worker_applied();
        mat.load_into(|c, k| {
            assert_eq!(c, b"CERTDATA");
            assert_eq!(k, b"KEYDATA");
        });

        // 2 台目（最後）適用 → 平文がゼロ化される。
        mat.worker_applied();
        mat.load_into(|c, k| {
            assert!(c.iter().all(|&b| b == 0), "cert must be zeroed");
            assert!(k.iter().all(|&b| b == 0), "key must be zeroed");
        });

        HTTP3_WORKER_COUNT.store(0, Ordering::SeqCst);
    }

    #[test]
    fn http3_publish_with_no_workers_is_noop() {
        let _g = HTTP3_TEST_LOCK.lock().unwrap();
        HTTP3_WORKER_COUNT.store(0, Ordering::SeqCst);

        let gen_before = HTTP3_CERT_GENERATION.load(Ordering::SeqCst);
        // ワーカー不在時は配信せず（世代を進めない）、平文は関数内で即ゼロ化される。
        publish_http3_certs(b"CERTDATA".to_vec(), b"KEYDATA".to_vec());
        let gen_after = HTTP3_CERT_GENERATION.load(Ordering::SeqCst);
        assert_eq!(gen_after, gen_before, "generation must not advance without workers");
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
