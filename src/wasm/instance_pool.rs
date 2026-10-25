//! VM の文脈の使い回し（F-271、`[[wasm.modules]] instance_reuse = true`）。
//!
//! `_start`・`proxy_on_vm_start`・`proxy_on_configure` を済ませたインスタンス（Store + Instance）をモジュールごとに
//! 持ち、要求ごとは http context の作成（`proxy_on_context_create`）とコールバックだけにする（Envoy と同じ寿命）。
//!
//! - 持つのはコールバックが成功したインスタンスだけ（トラップ・エラーのインスタンスは捨てて、次は作り直す）。
//! - ワーカー同士が同じロックを奪い合わないよう、スレッドごとに決めた区画（[`SHARDS`] 個）に分けて持つ。
//!   1 区画に持つのは [`PER_SHARD`] 個まで（プーリングアロケータの `total_memories` を使い切らない）。
//! - プールはモジュール（`LoadedModule`）が持つので、設定のリロードでモジュールが捨てられると一緒に消える。
//! - ルートごとにプラグインの設定（`configuration`）を上書きできるので、設定ごとに別のインスタンスにする
//!   （`on_configure` は作ったときの設定で 1 回だけ走るため）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use wasmtime::{Instance, Store};

use super::context::HostState;

/// 区画の数
const SHARDS: usize = 8;
/// 1 区画に持つインスタンスの数
const PER_SHARD: usize = 2;

/// 使い回すインスタンス
pub(crate) struct PooledInstance {
    pub(crate) store: Store<HostState>,
    pub(crate) instance: Instance,
    /// 作ったときのプラグインの設定（同じ設定の要求にだけ使う）
    pub(crate) configuration: Arc<[u8]>,
    /// 次の要求に使う http context の ID
    pub(crate) next_context_id: i32,
}

/// モジュールごとのプール
pub(crate) struct InstancePool {
    shards: Box<[Mutex<Vec<PooledInstance>>]>,
}

static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// このスレッドが使う区画（初めて使うときに決める）
    static SHARD: usize = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % SHARDS;
}

impl InstancePool {
    pub(crate) fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(Vec::new())).collect(),
        }
    }

    fn shard(&self) -> &Mutex<Vec<PooledInstance>> {
        &self.shards[SHARD.with(|s| *s)]
    }

    /// 同じ設定で作ったインスタンスを 1 つ取り出す
    pub(crate) fn take(&self, configuration: &Arc<[u8]>) -> Option<PooledInstance> {
        let mut v = self.shard().lock().unwrap_or_else(|e| e.into_inner());
        let i = v
            .iter()
            .rposition(|p| Arc::ptr_eq(&p.configuration, configuration))?;
        Some(v.swap_remove(i))
    }

    /// 戻す（区画が埋まっていれば捨てる）
    pub(crate) fn put(&self, p: PooledInstance) {
        let mut v = self.shard().lock().unwrap_or_else(|e| e.into_inner());
        if v.len() < PER_SHARD {
            v.push(p);
        }
    }

    /// 持っているインスタンスの数（テスト・診断用）
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).len())
            .sum()
    }
}
