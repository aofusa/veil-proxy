//! メッシュの状態（F-218）: リソース単位で差し替える表。
//!
//! - リスナーの表は 1 つの `ArcSwap<ListenerTable>`（LDS の更新で作り直す。接続の受理ごとに 1 回 `load`）。
//! - ルート表・クラスタは名前ごとのスロット（`Arc`）で、スロットの中身だけを差し替える。リスナー・ルートは
//!   スロットの `Arc` を持つので、要求ごとに名前で表を引かない。
//! - 書き込み（xDS の適用）は専用スレッドだけが行い、`registry` の `Mutex` はそのスレッドと起動時にしか取らない。

use super::model::*;
use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// リスナーの表。
#[derive(Debug, Default)]
pub struct ListenerTable {
    /// 自分でポートを開くリスナー（virtualOutbound・virtualInbound・ゲートウェイ）
    pub bound: Vec<Arc<MeshListener>>,
    /// `bind_to_port = false` のリスナー（元の宛先で渡される）: (IP, ポート) の完全一致
    pub by_addr: HashMap<SocketAddr, Arc<MeshListener>>,
    /// 同上: `0.0.0.0:<port>`（ワイルドカード）
    pub by_port: HashMap<u16, Arc<MeshListener>>,
    pub version: u64,
}

impl ListenerTable {
    /// 元の宛先に対応するリスナー（完全一致 → ポートだけ）。
    pub fn find_original_dst(&self, dst: SocketAddr) -> Option<&Arc<MeshListener>> {
        self.by_addr
            .get(&dst)
            .or_else(|| self.by_port.get(&dst.port()))
    }

    pub fn bound_by_port(&self, port: u16) -> Option<&Arc<MeshListener>> {
        self.bound.iter().find(|l| l.addr.port() == port)
    }
}

/// 名前で共有するスロットの登録簿（xDS の適用スレッドだけが触る）。
#[derive(Default)]
pub struct Registry {
    pub routes: HashMap<String, Arc<RouteSlot>>,
    pub clusters: HashMap<String, Arc<ClusterSlot>>,
    /// ECDS（HTTP フィルタの設定）のスロット
    pub ecds: HashMap<String, Arc<EcdsSlot>>,
}

impl Registry {
    pub fn route_slot(&mut self, name: &str) -> Arc<RouteSlot> {
        self.routes
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(RouteSlot::new(name)))
            .clone()
    }

    pub fn ecds_slot(&mut self, name: &str) -> Arc<EcdsSlot> {
        self.ecds
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(EcdsSlot {
                    name: name.to_string(),
                    filter: arc_swap::ArcSwapOption::empty(),
                })
            })
            .clone()
    }

    pub fn cluster_slot(&mut self, name: &str) -> Arc<ClusterSlot> {
        self.clusters
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(ClusterSlot::new(name)))
            .clone()
    }
}

pub struct MeshState {
    pub listeners: ArcSwap<ListenerTable>,
    pub registry: Mutex<Registry>,
    /// 最初の LDS・CDS が届いたか（`/ready`・pilot-agent の準備確認）
    pub lds_ready: AtomicBool,
    pub cds_ready: AtomicBool,
    pub lds_updates: AtomicU64,
    pub lds_rejected: AtomicU64,
    pub cds_updates: AtomicU64,
    pub cds_rejected: AtomicU64,
    pub rds_updates: AtomicU64,
    pub eds_updates: AtomicU64,
    /// 終了中（新しい接続を受けず、キープアライブを閉じる）
    pub draining: AtomicBool,
    /// inbound だけの排出（pilot-agent の `/drain_listeners?inboundonly`）
    pub draining_inbound: AtomicBool,
    pub xds_connected: AtomicBool,
}

pub static STATE: Lazy<MeshState> = Lazy::new(|| MeshState {
    listeners: ArcSwap::from_pointee(ListenerTable::default()),
    registry: Mutex::new(Registry::default()),
    lds_ready: AtomicBool::new(false),
    cds_ready: AtomicBool::new(false),
    lds_updates: AtomicU64::new(0),
    lds_rejected: AtomicU64::new(0),
    cds_updates: AtomicU64::new(0),
    cds_rejected: AtomicU64::new(0),
    rds_updates: AtomicU64::new(0),
    eds_updates: AtomicU64::new(0),
    draining: AtomicBool::new(false),
    draining_inbound: AtomicBool::new(false),
    xds_connected: AtomicBool::new(false),
});

pub fn state() -> &'static MeshState {
    &STATE
}

impl MeshState {
    pub fn is_draining(&self, direction: Direction) -> bool {
        self.draining.load(Ordering::Relaxed)
            || (direction == Direction::Inbound && self.draining_inbound.load(Ordering::Relaxed))
    }

    pub fn ready(&self) -> bool {
        self.lds_ready.load(Ordering::Relaxed) && self.cds_ready.load(Ordering::Relaxed)
    }
}
