//! リバリデーション追跡
//!
//! Cache Stampede防止のためのRequest Collapsingを実装します。
//! 同一キャッシュキーに対する重複した更新リクエストを防ぎ、
//! バックエンドへの過負荷を軽減します。

use dashmap::DashSet;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicU64, Ordering};

/// 更新中のキャッシュキーを追跡するDashSet
/// 
/// ハッシュ値（u64）をキーとして、現在バックグラウンドで
/// 更新処理中のキャッシュエントリを追跡します。
static REVALIDATING_KEYS: Lazy<DashSet<u64>> = Lazy::new(DashSet::new);

/// 合流されたリクエスト数（メトリクス用）
static COLLAPSED_REQUESTS: AtomicU64 = AtomicU64::new(0);

/// 更新開始を試みる
/// 
/// 同一キーに対して既に更新が進行中の場合は`false`を返し、
/// 呼び出し元は更新処理をスキップすべきです。
/// 
/// # Returns
/// 
/// - `true`: 更新を開始できる（このリクエストが担当）
/// - `false`: 別のリクエストが既に更新中（スキップ可能）
/// 
/// # Example
/// 
/// ```rust
/// if try_start_revalidation(cache_key.hash_value()) {
///     // バックグラウンド更新を実行
///     spawn_background_revalidation(...);
///     // 完了後にfinish_revalidationを呼ぶ
/// } else {
///     // 別のタスクが更新中なのでスキップ
/// }
/// ```
#[inline]
pub fn try_start_revalidation(hash: u64) -> bool {
    if REVALIDATING_KEYS.insert(hash) {
        true
    } else {
        // 別のタスクが更新中 - メトリクスを記録
        COLLAPSED_REQUESTS.fetch_add(1, Ordering::Relaxed);
        false
    }
}

/// 更新完了を記録
/// 
/// バックグラウンド更新が完了（成功・失敗問わず）したら必ず呼び出してください。
/// これにより、同一キーに対する次の更新が可能になります。
/// 
/// # Panics
/// 
/// この関数はパニックしません。キーが存在しない場合は何もしません。
#[inline]
pub fn finish_revalidation(hash: u64) {
    REVALIDATING_KEYS.remove(&hash);
}

/// 現在更新中のキー数を取得
#[inline]
pub fn active_revalidations() -> usize {
    REVALIDATING_KEYS.len()
}

/// 合流されたリクエスト数を取得（メトリクス用）
#[inline]
pub fn collapsed_request_count() -> u64 {
    COLLAPSED_REQUESTS.load(Ordering::Relaxed)
}

/// 統計情報をリセット
#[inline]
pub fn reset_stats() {
    COLLAPSED_REQUESTS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_collapsing() {
        let hash = 12345u64;
        
        // 最初のリクエストは成功
        assert!(try_start_revalidation(hash));
        
        // 重複リクエストは失敗
        assert!(!try_start_revalidation(hash));
        assert!(!try_start_revalidation(hash));
        
        // 合流カウントを確認
        assert!(collapsed_request_count() >= 2);
        
        // 完了後は再度開始可能
        finish_revalidation(hash);
        assert!(try_start_revalidation(hash));
        
        // クリーンアップ
        finish_revalidation(hash);
    }

    #[test]
    fn test_different_keys() {
        let hash1 = 111u64;
        let hash2 = 222u64;
        
        // 異なるキーは独立して更新可能
        assert!(try_start_revalidation(hash1));
        assert!(try_start_revalidation(hash2));
        
        finish_revalidation(hash1);
        finish_revalidation(hash2);
    }

    #[test]
    fn test_active_count() {
        let hash1 = 333u64;
        let hash2 = 444u64;
        
        let initial = active_revalidations();
        
        try_start_revalidation(hash1);
        assert_eq!(active_revalidations(), initial + 1);
        
        try_start_revalidation(hash2);
        assert_eq!(active_revalidations(), initial + 2);
        
        finish_revalidation(hash1);
        assert_eq!(active_revalidations(), initial + 1);
        
        finish_revalidation(hash2);
        assert_eq!(active_revalidations(), initial);
    }
}
