//! Graceful Shutdown・ホットリロードのフラグ、同時接続数、レートリミッター（B-31）
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// Graceful Shutdown / Hot Reload フラグ
// ====================

/// シャットダウンフラグ（Ctrl+C等でtrueに設定）
/// HTTP/3モジュールからも参照できるようにpub
pub static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

/// 設定リロード要求フラグ（SIGHUP でトリガー）
/// Arc<AtomicBool> として初期化（signal-hook の要件）
pub static RELOAD_FLAG: Lazy<Arc<AtomicBool>> = Lazy::new(|| Arc::new(AtomicBool::new(false)));

/// TLS 証明書リロード要求フラグ（SIGHUP でトリガー、F-03）
/// 設定リロードとは独立して証明書のみを再読み込みするために使用する。
pub static TLS_RELOAD_FLAG: Lazy<Arc<AtomicBool>> = Lazy::new(|| Arc::new(AtomicBool::new(false)));

/// グローバル IP ブロックリスト（最前線 DDoS 防御、F-35）。
///
/// `accept` 直後（TLS ハンドシェイク前・ハンドラ spawn 前）に評価し、ブロック対象 IP の
/// 接続を即座に切断することで、既知の不正 IP に対する TLS ハンドシェイク等の高コスト処理を
/// 行わずに弾く。CIDR は起動時にパース済み（`CidrRange`）で保持し、accept ホットパスでは
/// 文字列パースを行わない。`ArcSwap` により SIGHUP ホットリロードに対応する。
///
/// 注: NIC ドライバ段でドロップする XDP/eBPF（F-35 の本来の設計）は、ビルドに nightly +
/// bpf-linker、実行に CAP_BPF/対応 NIC を要し本リポジトリのサンドボックスでは検証不能なため、
/// 検証可能な「ユーザースペース最前線（accept 段の事前ドロップ）」をまず実装する。
pub static GLOBAL_BLOCKED_IPS: Lazy<ArcSwap<Vec<CidrRange>>> =
    Lazy::new(|| ArcSwap::from_pointee(Vec::new()));

/// グローバル IP ブロックリストを設定する（起動時・SIGHUP リロード時に呼ぶ）。
/// パース不能なエントリは黙って無視する（設定検証は別途 `validate` で行う）。
pub fn set_global_blocked_ips(cidrs: &[String]) {
    let parsed: Vec<CidrRange> = cidrs.iter().filter_map(|s| CidrRange::parse(s)).collect();
    GLOBAL_BLOCKED_IPS.store(Arc::new(parsed));
}

/// 指定 IP がグローバルブロックリストに含まれるか（accept ホットパス、ゼロアロケーション）。
///
/// ブロックリスト未設定（空）時は即 `false` を返し、ホットパスのオーバーヘッドを発生させない。
#[inline]
pub fn is_ip_blocked(ip: std::net::IpAddr) -> bool {
    let list = GLOBAL_BLOCKED_IPS.load();
    if list.is_empty() {
        return false;
    }
    list.iter().any(|cidr| cidr.contains_addr(ip))
}

/// グレースフルシャットダウンタイムアウト（秒）
/// 起動時に設定ファイルから読み込まれ、ワーカースレッドがドレイン待機時に参照
pub static GRACEFUL_SHUTDOWN_TIMEOUT_SECS: AtomicU64 = AtomicU64::new(30);

// secure_zero, secure_clear_arc_vec は crate::system モジュールに移動しました。

// ====================
// 同時接続数カウンター
// ====================
//
// グローバルなアトミックカウンターで現在の接続数を追跡します。
// max_concurrent_connections が設定されている場合、上限を超える接続は拒否されます。
// ====================

pub static CURRENT_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// グレースフルシャットダウン時に既存接続の完了を待機
///
/// CURRENT_CONNECTIONSが0になるか、タイムアウトに達するまで待機します。
/// 設定されたタイムアウト（GRACEFUL_SHUTDOWN_TIMEOUT_SECS）を使用。
///
/// # Arguments
/// * `worker_type` - ワーカーの種類（ログ表示用）
/// * `thread_id` - スレッドID（ログ表示用）
pub async fn drain_connections(worker_type: &str, thread_id: usize) {
    let timeout_secs = GRACEFUL_SHUTDOWN_TIMEOUT_SECS.load(Ordering::Relaxed);

    if timeout_secs == 0 {
        // タイムアウト0の場合は即座に終了（既存の動作）
        return;
    }

    // F-187: キープアライブで次の要求を待っているだけの接続（このワーカーが受け付けたもの）を
    // すぐ閉じさせる。処理中の要求は待ち、応答を返して次の待ちに入った時点で閉じる。
    crate::runtime::time::expire_idle_timeouts();

    let start = std::time::Instant::now();
    let drain_timeout = Duration::from_secs(timeout_secs);

    // 初期接続数を確認
    let initial_connections = CURRENT_CONNECTIONS.load(Ordering::Relaxed);
    if initial_connections == 0 {
        info!(
            "[{} {}] No active connections, proceeding with shutdown",
            worker_type, thread_id
        );
        return;
    }

    info!(
        "[{} {}] Draining {} active connections (timeout: {}s)...",
        worker_type, thread_id, initial_connections, timeout_secs
    );

    // 接続数が0になるかタイムアウトするまで待機
    while CURRENT_CONNECTIONS.load(Ordering::Relaxed) > 0 {
        if start.elapsed() > drain_timeout {
            let remaining = CURRENT_CONNECTIONS.load(Ordering::Relaxed);
            warn!(
                "[{} {}] Drain timeout after {}s, {} connections still active (forcing shutdown)",
                worker_type, thread_id, timeout_secs, remaining
            );
            break;
        }
        crate::runtime::time::sleep(Duration::from_millis(100)).await;
    }

    let elapsed = start.elapsed();
    let remaining = CURRENT_CONNECTIONS.load(Ordering::Relaxed);
    if remaining == 0 {
        info!(
            "[{} {}] All connections drained in {:.1}s",
            worker_type,
            thread_id,
            elapsed.as_secs_f64()
        );
    }
}

// ====================
// レートリミッター（スライディングウィンドウ方式、プロセス共有。B-31）
// ====================
//
// クライアント IP ごとの分間リクエスト数を、**全ワーカーで共有する固定サイズの表**で数える。
// 従来はワーカーごとの `thread_local!` だったため、設定した「IP あたり N 回/分」が
// ワーカー数倍まで通っていた（B-31）。
//
// - 表は 65,536 スロットの `AtomicU64`（512KB）。レートリミットを使う最初の要求で確保する。
// - スロットは IP のハッシュ（プロセスごとの乱数シード）で選ぶ。別の IP と同じスロットに
//   入った場合はカウントを共有する（= わずかに厳しめに倒れる。上限を超えて通すことはない）。
// - 各スロットは `(分, 今の分の件数, 前の分の件数)` を 64bit に詰め、CAS 1 回で更新する。
//   要求ごとの確保は無い（従来は新しい IP ごとに `to_string()` していた）。
// - 推定レートは従来と同じスライディングウィンドウ:
//   `前の分の件数 × (今の分の残り秒 / 60) + 今の分の件数`
// ====================

/// 表のスロット数（2 の冪）。
pub(super) const RATE_SLOTS: usize = 1 << 16;

pub(super) struct RateTable {
    slots: Box<[std::sync::atomic::AtomicU64]>,
    hasher: foldhash::quality::RandomState,
}

pub(super) static RATE_TABLE: std::sync::LazyLock<RateTable> =
    std::sync::LazyLock::new(|| RateTable {
        slots: (0..RATE_SLOTS)
            .map(|_| std::sync::atomic::AtomicU64::new(0))
            .collect(),
        hasher: foldhash::quality::RandomState::default(),
    });

/// スロットの値 `(分, 今の分の件数, 前の分の件数)` を詰める（分は下位 32bit）。
#[inline]
pub(super) fn rate_pack(minute: u32, current: u16, previous: u16) -> u64 {
    (minute as u64) | ((current as u64) << 32) | ((previous as u64) << 48)
}

#[inline]
pub(super) fn rate_unpack(v: u64) -> (u32, u16, u16) {
    (v as u32, (v >> 32) as u16, (v >> 48) as u16)
}

/// スロットへ 1 件記録し、推定レート（スライディングウィンドウ）を返す。
pub(super) fn rate_record(
    slot: &std::sync::atomic::AtomicU64,
    minute: u32,
    second_in_minute: u32,
) -> u32 {
    use std::sync::atomic::Ordering;
    let mut old = slot.load(Ordering::Relaxed);
    loop {
        let (m, cur, prev) = rate_unpack(old);
        let (cur2, prev2) = if m == minute {
            (cur.saturating_add(1), prev)
        } else if m.wrapping_add(1) == minute {
            (1, cur)
        } else {
            (1, 0)
        };
        match slot.compare_exchange_weak(
            old,
            rate_pack(minute, cur2, prev2),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                let weight = 60u32.saturating_sub(second_in_minute) as u64;
                // ceil(prev × weight / 60) + cur
                let carried = (prev2 as u64 * weight).div_ceil(60);
                return (carried + cur2 as u64).min(u32::MAX as u64) as u32;
            }
            Err(actual) => old = actual,
        }
    }
}

/// レートリミットをチェック（全ワーカー共有）。
///
/// `zone` はルートごとの識別子（呼び出し側はルートの `SecurityConfig` のアドレスを渡す）。
/// 件数は `(ルート, IP)` ごとに数える（あるルートへの要求が別のルートの上限を消費しない。
/// nginx の `limit_req` のゾーンと同じ考え方）。設定のリロードで新しいルートになると数え直す。
/// 戻り値: レート制限内であれば true
pub fn check_rate_limit(zone: usize, client_ip: &str, limit: u64) -> bool {
    if limit == 0 {
        return true; // 0 = 無制限
    }
    let table = &*RATE_TABLE;
    let idx = (std::hash::BuildHasher::hash_one(&table.hasher, (zone, client_ip)) as usize)
        & (RATE_SLOTS - 1);
    // Coarse Timer（システムコール削減。精度 100ms 程度で十分）
    let now_secs = coarse_now().unix_timestamp() as u64;
    let rate = rate_record(
        &table.slots[idx],
        (now_secs / 60) as u32,
        (now_secs % 60) as u32,
    );
    rate as u64 <= limit
}

#[cfg(test)]
pub(super) mod b31_rate_limit_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn pack_roundtrip() {
        let v = rate_pack(29_000_000, 123, 65_535);
        assert_eq!(rate_unpack(v), (29_000_000, 123, 65_535));
    }

    /// 同じ分は加算、次の分は前の分へ繰り越し、2 分以上空いたらリセット。
    #[test]
    fn sliding_window() {
        let slot = AtomicU64::new(0);
        for _ in 0..10 {
            rate_record(&slot, 100, 0);
        }
        assert_eq!(
            rate_unpack(slot.load(std::sync::atomic::Ordering::Relaxed)).1,
            10
        );
        // 次の分の 30 秒目: 前の分 10 件の半分（5）+ 今の分 1 件。
        assert_eq!(rate_record(&slot, 101, 30), 6);
        // 分の終わり近く: 繰り越しはほぼ 0（切り上げで 1）+ 今の分 2 件。
        assert_eq!(rate_record(&slot, 101, 59), 3);
        // 2 分以上空いたらリセット。
        assert_eq!(rate_record(&slot, 105, 0), 1);
    }

    /// 全ワーカー共有: 別スレッドからの要求も同じ IP として数える。
    #[test]
    fn shared_across_threads() {
        let ip = "203.0.113.77";
        let limit = 40;
        let allowed: usize = (0..4)
            .map(|_| {
                std::thread::spawn(move || {
                    (0..20).filter(|_| check_rate_limit(1, ip, limit)).count()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .sum();
        // 4 スレッド × 20 = 80 件のうち、通るのは上限（40）まで（分の境目をまたぐと
        // 前の分の繰り越しで少し減ることがあるので上限側だけを厳密に見る）。
        assert!(
            allowed <= limit as usize,
            "allowed {} > limit {}",
            allowed,
            limit
        );
        assert!(allowed >= 1);
    }

    /// ルート（zone）が違えば件数は別々に数える。
    #[test]
    fn zones_are_independent() {
        let ip = "198.51.100.9";
        assert!((0..5).all(|_| check_rate_limit(11, ip, 5)));
        assert!(!check_rate_limit(11, ip, 5));
        assert!(check_rate_limit(12, ip, 5));
    }
}
