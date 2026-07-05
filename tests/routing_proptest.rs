//! F-56: ルーティングのプロパティベーステスト
//!
//! `veil::routing` のマッチング経路は、外部から与えられる任意の Host / Path / 送信元 IP を
//! 受け取るデータプレーンの入口である。ここでは proptest で任意入力を大量生成し、
//! 次の **不変条件** を検証する（テストを通すためだけの空テストではなく、実際に
//! panic・順序破壊・非決定性・キャッシュ不整合を検出する）。
//!
//! 1. `OptimizedRouter::get_candidates` は任意入力で panic せず、結果が
//!    昇順ソート済み・重複なし・全インデックスが `route_count` 未満で、
//!    同一入力に対して決定的（2 回呼んで同一）であること。
//! 2. Host サフィックスワイルドカード `*.example.com` は「ちょうど 1 ラベルの
//!    サブドメイン」だけにマッチし、ベースドメインや多段サブドメインには
//!    マッチしないこと。
//! 3. Path プレフィックスワイルドカード `/api/*` は `/api` 配下のパスにのみ
//!    マッチし、境界（`/apix` 等）を誤って拾わないこと。
//! 4. `RouteCache`（xxh3 キー + スレッドローカル LRU）は put した値を同一キーで
//!    get すると必ず取り出せること（キャッシュ整合）。

use proptest::prelude::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use veil::routing::{HostRouter, OptimizedRouter, PathRouter, RouteCache, RouteCacheKey};

/// 代表的なルート集合を持つ最適化ルータを構築する。
///
/// - route 0: 任意 Host / `/api/*` / 任意 IP
/// - route 1: `*.example.com` / 任意 Path / 任意 IP
/// - route 2: `localhost` / `/health` / 任意 IP
/// - route 3: 任意 Host / 任意 Path / `10.0.0.0/8`
/// - route 4: catch-all（すべて None）
const ROUTE_COUNT: usize = 5;

fn build_router() -> OptimizedRouter {
    let mut r = OptimizedRouter::new();
    r.add_route(0, None, Some("/api/*"), None);
    r.add_route(1, Some("*.example.com"), None, None);
    r.add_route(2, Some("localhost"), Some("/health"), None);
    r.add_route(3, None, None, Some(&["10.0.0.0/8".to_string()]));
    r.add_route(4, None, None, None);
    r.finalize();
    r
}

fn sockaddr(strategy_bits: u32, port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::from(strategy_bits)), port)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// 不変条件 1: get_candidates は panic せず、結果は昇順・重複なし・範囲内・決定的。
    #[test]
    fn get_candidates_invariants(
        host in "[a-zA-Z0-9.*:_-]{0,40}",
        path in "/[a-zA-Z0-9./*{}:_-]{0,40}",
        ip_bits in any::<u32>(),
        port in any::<u16>(),
    ) {
        let router = build_router();
        let addr = sockaddr(ip_bits, port);

        let first = router.get_candidates(&host, &path, &addr);

        // 昇順ソート済みかつ重複なし（get_candidates は sort_unstable + dedup を保証）。
        for w in first.windows(2) {
            prop_assert!(w[0] < w[1], "candidates must be strictly ascending: {:?}", first);
        }
        // 全インデックスが route_count 未満。
        for &idx in &first {
            prop_assert!(idx < ROUTE_COUNT, "index {} out of range", idx);
        }
        // 決定性: 同一入力での 2 回目が完全一致。
        let second = router.get_candidates(&host, &path, &addr);
        prop_assert_eq!(first, second);
    }

    /// 不変条件 2a: ちょうど 1 ラベルのサブドメインは `*.example.com` にマッチ。
    #[test]
    fn host_wildcard_matches_single_label_subdomain(
        label in "[a-z0-9][a-z0-9-]{0,20}",
    ) {
        let mut hr = HostRouter::new();
        hr.add_route(1, Some("*.example.com"));
        let host = format!("{label}.example.com");
        let cands = hr.get_candidates(&host);
        prop_assert!(cands.contains(&1), "'{host}' should match *.example.com, got {cands:?}");
    }

    /// 不変条件 2b: ベースドメイン・多段サブドメインは `*.example.com` にマッチしない。
    #[test]
    fn host_wildcard_rejects_base_and_multilabel(
        l1 in "[a-z0-9]{1,10}",
        l2 in "[a-z0-9]{1,10}",
    ) {
        let mut hr = HostRouter::new();
        hr.add_route(1, Some("*.example.com"));
        // ベースドメインそのもの（サブドメイン無し）。
        prop_assert!(!hr.get_candidates("example.com").contains(&1));
        // 2 段サブドメイン（ドットを含むラベル）。
        let multi = format!("{l1}.{l2}.example.com");
        prop_assert!(
            !hr.get_candidates(&multi).contains(&1),
            "'{multi}' must not match single-label wildcard"
        );
    }

    /// 不変条件 3: `/api/*` は `/api` 配下のみにマッチし、境界を誤検出しない。
    #[test]
    fn path_wildcard_prefix_boundary(
        rest in "[a-z0-9/_-]{0,30}",
        other in "[a-z0-9_-]{1,20}",
    ) {
        let mut pr = PathRouter::new();
        pr.add_route(0, Some("/api/*"));

        // /api 配下（/api/... または /api ちょうど）はマッチ。
        let under = format!("/api/{rest}");
        prop_assert!(pr.get_candidates(&under).contains(&0), "'{under}' should match /api/*");

        // /apiXXX（スラッシュ区切りでない）は境界を越えないためマッチしない。
        prop_assume!(!other.starts_with('/'));
        let sibling = format!("/api{other}");
        prop_assert!(
            !pr.get_candidates(&sibling).contains(&0),
            "'{sibling}' must not match /api/* (word-boundary)"
        );
    }

    /// 不変条件 4: put した値は同一キーで必ず取り出せる（キャッシュ整合）。
    ///
    /// 本ファイル内でキャッシュ（スレッドローカル + グローバル世代）へ触れるのは
    /// このテストのみとし、他テストとの世代干渉を避ける。
    #[test]
    fn route_cache_put_then_get_is_coherent(
        host in "[a-z0-9.]{0,30}",
        path in "/[a-z0-9./]{0,30}",
        method in prop::sample::select(vec!["GET", "POST", "PUT", "DELETE"]),
        ip_bits in any::<u32>(),
        route in prop::option::of(0usize..ROUTE_COUNT),
    ) {
        let cache = RouteCache::new(10_000);
        let addr = sockaddr(ip_bits, 0);
        let key = RouteCacheKey::new(host.as_bytes(), path.as_bytes(), method.as_bytes(), &addr);

        cache.put(key, route);
        // 同一入力から再構築したキーでヒットし、格納値と一致すること。
        let key2 = RouteCacheKey::new(host.as_bytes(), path.as_bytes(), method.as_bytes(), &addr);
        prop_assert_eq!(cache.get(&key2), Some(route));
    }
}
