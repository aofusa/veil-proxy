//! Proxy-Wasm の shared data の表（B-179）
//!
//! 同じモジュール（名前）の全インスタンス・全ワーカーで共有するキー・値の表。Envoy の `vm_id` ごとの shared data に当たる。
//! 表はモジュール名ごとにプロセスで 1 つ持ち、設定のリロードで作り直したモジュールにも引き継ぐ（Envoy と同じ）。
//! モジュールのロード（起動・リロード）で `for_module` から `Arc` を取り、要求ごとの http context には `Arc` の clone を渡す。
//!
//! 値は `Arc<[u8]>` で持ち、読み取りは参照を増やすだけで返す。表はキーのハッシュで `SHARDS` 個に分け、ロックは
//! 1 回のハッシュ表の操作の間だけ持つ（`await` をまたがない。書き込み同士・読み書きが同じ区画に重なったときだけ待つ）。

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use once_cell::sync::Lazy;

const SHARDS: usize = 16;

/// 1 モジュール分の shared data
pub struct SharedData {
    shards: [RwLock<HashMap<Box<str>, (Arc<[u8]>, u32)>>; SHARDS],
    /// 次に払い出す CAS の値（0 は「CAS を検査しない」の意味なので使わない）
    next_cas: AtomicU32,
}

/// `set` の結果
#[derive(Debug, PartialEq, Eq)]
pub enum SetResult {
    Ok,
    CasMismatch,
}

impl Default for SharedData {
    fn default() -> Self {
        Self {
            shards: std::array::from_fn(|_| RwLock::new(HashMap::new())),
            next_cas: AtomicU32::new(1),
        }
    }
}

impl SharedData {
    fn shard(&self, key: &str) -> &RwLock<HashMap<Box<str>, (Arc<[u8]>, u32)>> {
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        &self.shards[(h.finish() as usize) % SHARDS]
    }

    /// 値と CAS を返す
    pub fn get(&self, key: &str) -> Option<(Arc<[u8]>, u32)> {
        let shard = self.shard(key).read().unwrap_or_else(|e| e.into_inner());
        shard.get(key).map(|(v, c)| (v.clone(), *c))
    }

    /// 値を置く。`cas` が 0 でなく、キーがあってその CAS と違えば `CasMismatch`（Proxy-Wasm の `proxy_set_shared_data`）
    pub fn set(&self, key: &str, value: &[u8], cas: u32) -> SetResult {
        let value: Arc<[u8]> = Arc::from(value);
        let mut shard = self.shard(key).write().unwrap_or_else(|e| e.into_inner());
        if cas != 0 {
            if let Some((_, current)) = shard.get(key) {
                if *current != cas {
                    return SetResult::CasMismatch;
                }
            }
        }
        let mut new_cas = self.next_cas.fetch_add(1, Ordering::Relaxed);
        if new_cas == 0 {
            new_cas = self.next_cas.fetch_add(1, Ordering::Relaxed);
        }
        match shard.get_mut(key) {
            Some(slot) => *slot = (value, new_cas),
            None => {
                shard.insert(Box::from(key), (value, new_cas));
            }
        }
        SetResult::Ok
    }

    /// 入っているキーの数（試験用）
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.read().unwrap_or_else(|e| e.into_inner()).len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// モジュール名 → 表。モジュールのロード（起動・リロード）のときだけ引く（データプレーンからは引かない）
static REGISTRY: Lazy<Mutex<HashMap<Box<str>, Arc<SharedData>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// モジュール名の表を返す（無ければ作る。リロードで同じ名前のモジュールを作り直しても同じ表を返す）
pub fn for_module(name: &str) -> Arc<SharedData> {
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    reg.entry(Box::from(name)).or_default().clone()
}
