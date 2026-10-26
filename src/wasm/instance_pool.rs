//! VM の文脈の使い回し（F-271、`[[wasm.modules]] instance_reuse = true`）。
//!
//! `_start`・`proxy_on_vm_start`・`proxy_on_configure` を済ませたインスタンス（Store + Instance）をワーカーの
//! スレッドごとに持つ。1 つのインスタンスに複数の要求の http context が同時に住む（Envoy の 1 ワーカー 1 VM と同じ）。
//!
//! - 要求は最初のコールバックでインスタンスを 1 つ選び（空いているもの、無ければ新しく作る）、そこに http context を
//!   作る。以後のフェーズ（本文・応答・`on_log`）は同じインスタンスの同じ context で呼び、要求が終わったら
//!   `proxy_on_delete` する（B-177。[`RequestSessions`]）。
//! - コールバックの間はインスタンスを取り出して使う（`&mut Store`）。同じインスタンスに context を持つ別の要求が
//!   その間にフェーズを呼んだら、戻されるまで待つ（[`InstanceSlot::acquire`]。ワーカーのスレッドの中だけで、
//!   ロックは使わない。B-177）。
//! - トラップ・エラーのインスタンスは捨てる。そこに context を持っていた要求の以後のフェーズはエラーになる。
//! - スレッドごとに持つ数の上限は [`per_thread_capacity`]（プーリングアロケータの `total_memories` を使い切らない）。
//!   上限を超えて作ったインスタンスはその要求だけが使い、要求が終われば捨てる。
//! - ルートごとにプラグインの設定（`configuration`）を上書きできるので、設定ごとに別のインスタンスにする
//!   （`on_configure` は作ったときの設定で 1 回だけ走るため）。

use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Poll, Waker};

use wasmtime::{Instance, Store};

use super::context::HostState;

/// スレッドごとに持つインスタンスの数の下限
const MIN_PER_THREAD: usize = 2;

/// ワーカーのスレッドの数の見積もり（上限の計算だけに使う）
fn worker_threads_hint() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// 1 スレッドに持つインスタンスの数の上限。
///
/// B-175: 以前は 8 区画 × 2 個に固定していた。1 ワーカーで同時に開いているインスタンスが 3 個以上になると、
/// 戻せない分を捨てて次の要求で `_start`・`on_configure` から作り直していた（プーリングアロケータの decommit の
/// `madvise` とメモリイメージの `mprotect` も伴う）。プーリングアロケータの `total_memories` の半分を、
/// 使い回すモジュールとワーカーの数で分けた数まで持つ（残りの半分は上限を超えたときに作る分）。
pub(crate) fn per_thread_capacity(total_memories: u32, reuse_modules: usize) -> usize {
    let n = total_memories as usize / (2 * worker_threads_hint() * reuse_modules.max(1));
    n.max(MIN_PER_THREAD)
}

/// 使い回すインスタンス
pub(crate) struct PooledInstance {
    pub(crate) store: Store<HostState>,
    pub(crate) instance: Instance,
    /// 次の要求に使う http context の ID
    pub(crate) next_context_id: i32,
}

/// 1 つのインスタンスと、そこに住む http context の管理（ワーカーのスレッドの中だけで使う）
pub(crate) struct InstanceSlot {
    /// コールバックの間は取り出されて `None`
    inst: RefCell<Option<PooledInstance>>,
    /// 取り出されている間に待つ要求
    waiters: RefCell<Vec<Waker>>,
    /// トラップ等で捨てた
    dead: Cell<bool>,
    /// 要求が `on_log` まで行かずに終わった context（次に使うときに `proxy_on_delete` する）
    pending_delete: RefCell<Vec<i32>>,
    /// 作ったときのプラグインの設定（同じ設定の要求にだけ使う）
    pub(crate) configuration: Arc<[u8]>,
}

impl InstanceSlot {
    /// 作ったばかりで最初のコールバックに使っている（取り出した状態の）インスタンスの枠
    pub(crate) fn new_in_use(configuration: Arc<[u8]>) -> Rc<Self> {
        Rc::new(Self {
            inst: RefCell::new(None),
            waiters: RefCell::new(Vec::new()),
            dead: Cell::new(false),
            pending_delete: RefCell::new(Vec::new()),
            configuration,
        })
    }

    /// 空いていれば取り出す
    fn try_take(&self) -> Option<PooledInstance> {
        if self.dead.get() {
            return None;
        }
        self.inst.borrow_mut().take()
    }

    /// インスタンスを取り出す（他の要求のコールバックの途中なら戻されるまで待つ）。捨てられていれば `None`
    pub(crate) async fn acquire(&self) -> Option<PooledInstance> {
        poll_fn(|cx| {
            if self.dead.get() {
                return Poll::Ready(None);
            }
            match self.inst.borrow_mut().take() {
                Some(i) => Poll::Ready(Some(i)),
                None => {
                    let mut w = self.waiters.borrow_mut();
                    if !w.iter().any(|x| x.will_wake(cx.waker())) {
                        w.push(cx.waker().clone());
                    }
                    Poll::Pending
                }
            }
        })
        .await
    }

    /// コールバックが済んだインスタンスを戻す
    pub(crate) fn release(&self, inst: PooledInstance) {
        *self.inst.borrow_mut() = Some(inst);
        self.wake();
    }

    /// トラップ・エラーのインスタンスを捨てる（待っている要求はエラーになる）
    pub(crate) fn discard(&self) {
        self.dead.set(true);
        self.wake();
    }

    fn wake(&self) {
        let w = std::mem::take(&mut *self.waiters.borrow_mut());
        for x in w {
            x.wake();
        }
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.dead.get()
    }

    /// 次に使うときに `proxy_on_delete` する context を取り出す
    pub(crate) fn take_pending_deletes(&self) -> Vec<i32> {
        if self.pending_delete.borrow().is_empty() {
            return Vec::new();
        }
        std::mem::take(&mut *self.pending_delete.borrow_mut())
    }
}

static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);

/// モジュールごとのプール（中身はワーカーのスレッドごと）
pub(crate) struct InstancePool {
    id: u64,
    per_thread: usize,
    /// モジュールが捨てられたら他のスレッドの持ち分も次に触ったときに捨てる
    alive: Arc<()>,
}

struct LocalEntry {
    id: u64,
    alive: std::sync::Weak<()>,
    slots: Vec<Rc<InstanceSlot>>,
}

thread_local! {
    static LOCAL: RefCell<Vec<LocalEntry>> = const { RefCell::new(Vec::new()) };
}

impl InstancePool {
    pub(crate) fn new(per_thread: usize) -> Self {
        Self {
            id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
            per_thread: per_thread.max(1),
            alive: Arc::new(()),
        }
    }

    fn with_local<R>(&self, f: impl FnOnce(&mut Vec<Rc<InstanceSlot>>) -> R) -> R {
        LOCAL.with(|l| {
            let mut l = l.borrow_mut();
            // リロードで捨てられたモジュールの持ち分を捨てる
            l.retain(|e| e.alive.strong_count() > 0);
            let i = match l.iter().position(|e| e.id == self.id) {
                Some(i) => i,
                None => {
                    l.push(LocalEntry {
                        id: self.id,
                        alive: Arc::downgrade(&self.alive),
                        slots: Vec::new(),
                    });
                    l.len() - 1
                }
            };
            f(&mut l[i].slots)
        })
    }

    /// 同じ設定で作った空いているインスタンスを 1 つ取り出す
    pub(crate) fn take_free(
        &self,
        configuration: &Arc<[u8]>,
    ) -> Option<(Rc<InstanceSlot>, PooledInstance)> {
        self.with_local(|slots| {
            slots.retain(|s| !s.is_dead());
            for s in slots.iter() {
                if Arc::ptr_eq(&s.configuration, configuration) {
                    if let Some(i) = s.try_take() {
                        return Some((s.clone(), i));
                    }
                }
            }
            None
        })
    }

    /// 新しく作ったインスタンスを登録する（上限を超えていれば登録せず、その要求だけが使う）
    pub(crate) fn register(&self, slot: &Rc<InstanceSlot>) {
        self.with_local(|slots| {
            if slots.len() < self.per_thread {
                slots.push(slot.clone());
            }
        })
    }

    /// このスレッドが持っているインスタンスの数（テスト・診断用）
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.with_local(|slots| slots.iter().filter(|s| !s.is_dead()).count())
    }

    /// このスレッドのインスタンスに溜まっている、消す予定の context の数（テスト用）
    #[cfg(test)]
    pub(crate) fn pending_deletes(&self) -> usize {
        self.with_local(|slots| slots.iter().map(|s| s.pending_delete.borrow().len()).sum())
    }
}

/// 1 つの要求が `instance_reuse` のモジュールに持つ http context（B-177）。
///
/// 要求の最初のフェーズで作り、`on_log`（[`super::engine::FilterEngine::on_log_with_modules_async`]）で
/// `proxy_on_delete` して空にする。そこまで行かずに捨てられたら、context はインスタンスの次の利用時に消す。
#[derive(Default)]
pub struct RequestSessions {
    entries: RefCell<Vec<SessionEntry>>,
}

pub(crate) struct SessionEntry {
    /// モジュール（`LoadedModule` のアドレス）
    pub(crate) module: usize,
    pub(crate) slot: Rc<InstanceSlot>,
    pub(crate) context_id: i32,
}

impl RequestSessions {
    pub(crate) fn find(&self, module: usize, configuration: &Arc<[u8]>) -> Option<(Rc<InstanceSlot>, i32)> {
        self.entries
            .borrow()
            .iter()
            .find(|e| e.module == module && Arc::ptr_eq(&e.slot.configuration, configuration))
            .map(|e| (e.slot.clone(), e.context_id))
    }

    pub(crate) fn insert(&self, module: usize, slot: Rc<InstanceSlot>, context_id: i32) {
        self.entries.borrow_mut().push(SessionEntry {
            module,
            slot,
            context_id,
        });
    }

    /// 終わった（`proxy_on_delete` 済み、またはインスタンスを捨てた）context を外す
    pub(crate) fn remove(&self, module: usize, slot: &Rc<InstanceSlot>) {
        self.entries
            .borrow_mut()
            .retain(|e| !(e.module == module && Rc::ptr_eq(&e.slot, slot)));
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.borrow().len()
    }
}

impl Drop for RequestSessions {
    fn drop(&mut self) {
        for e in self.entries.get_mut().drain(..) {
            if !e.slot.is_dead() {
                e.slot.pending_delete.borrow_mut().push(e.context_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_thread_capacity_leaves_half_of_total_memories() {
        let n = worker_threads_hint();
        assert_eq!(per_thread_capacity(1000 * n as u32, 1), 500);
        assert_eq!(per_thread_capacity(1000 * n as u32, 5), 100);
        // 小さい total_memories でも下限は持つ
        assert_eq!(per_thread_capacity(1, 1), MIN_PER_THREAD);
        assert_eq!(per_thread_capacity(0, 0), MIN_PER_THREAD);
    }

    #[test]
    fn dropped_sessions_queue_context_deletes_on_live_slots_only() {
        let cfg: Arc<[u8]> = Arc::from(&b""[..]);
        let live = InstanceSlot::new_in_use(cfg.clone());
        let dead = InstanceSlot::new_in_use(cfg.clone());
        dead.discard();
        let s = RequestSessions::default();
        s.insert(1, live.clone(), 7);
        s.insert(2, dead.clone(), 8);
        assert_eq!(s.find(1, &cfg).map(|(_, id)| id), Some(7));
        // 別の設定の context は別物
        let other: Arc<[u8]> = Arc::from(&b"x"[..]);
        assert!(s.find(1, &other).is_none());
        drop(s);
        assert_eq!(live.take_pending_deletes(), vec![7]);
        assert!(live.take_pending_deletes().is_empty());
        assert!(dead.pending_delete.borrow().is_empty());
    }

    #[test]
    fn removed_session_is_not_queued() {
        let cfg: Arc<[u8]> = Arc::from(&b""[..]);
        let slot = InstanceSlot::new_in_use(cfg);
        let s = RequestSessions::default();
        s.insert(1, slot.clone(), 3);
        s.remove(1, &slot);
        assert_eq!(s.len(), 0);
        drop(s);
        assert!(slot.take_pending_deletes().is_empty());
    }
}
