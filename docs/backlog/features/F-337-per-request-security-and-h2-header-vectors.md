# F-337: 要求ごとの IP 制限の作り直しと HTTP/2 の経路の解決用のヘッダの `Vec` をなくす

**状態: 未着手（[F-327](F-327-proxy-wasm-path-cost.md) の調査で見つけた。Wasm に限らない。2026-11-26）**

## 現状

- `l7::check_security`（`src/l7.rs`）が要求ごとに `SecurityConfig::ip_filter()`（`src/config/net_policy.rs`）で `IpFilter` を作り直す。
  `IpFilter::from_lists` は `allowed_ips`・`denied_ips` の文字列を毎回 `CidrRange::parse` して `Vec` に集める。一覧が空なら確保は無いが、
  IP 制限を書いたルートでは要求ごとに確保と解析が走る（ホットパスの規則に反する）。
- HTTP/2 の要求は、経路の解決に渡すヘッダを要求ごとに `Vec<(&[u8], &[u8])>` へ集める（`src/proxy/h2.rs` の `h2_dispatch`・
  `h2_serve_streaming`。`h2_route_streaming_plan` は `Bytes` の組の `Vec` も作る）。HTTP/1.1 は 16 個までスタックの配列
  （`proxy::h1::handle_requests` の `raw_buf`）。

## 案

1. `IpFilter` を設定のロードの時に解決し、ルート（`Route` の `#[serde(skip)]` のフィールド、F-148/F-159）か `SecurityConfig` に持つ。
   リロードは設定ごと作り直すので、要求の経路は参照だけにする。
2. HTTP/2 の経路の解決はヘッダの表（`stream.request_headers`）を直接見る形にするか、HTTP/1.1 と同じく小さな数までスタックの配列にする。

## 受け入れ条件

- IP 制限のあるルート・無いルートの両方で、1 要求の確保の回数が減る（`allocs.py`）。挙動は変えない（既存の IP 制限・経路の解決の試験）。
- HTTP/2（TLS・h2c）の交互 A/B で退行が無い（両バックエンド）。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（調査で見つけた）・B-322（許可リストが 1 つも読めないときは全部拒否）
