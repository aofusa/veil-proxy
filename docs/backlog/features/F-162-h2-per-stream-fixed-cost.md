# F-162: HTTP/2 per-stream 固定費の削減（F-157 Phase 3 の io_uring 側）

**優先度**: P2
**ステータス**: 完了（h2c_file 3B **+0.89%**、h2c_proxy 54KB は 18 ラウンドで **-0.29%＝ノイズ内**）
**関連**: F-157（h2c の対 nginx 劣後解消）、F-158（インライン初回 poll は io_uring では不採用）

---

## 背景

F-158 で「per-stream タスクの spawn 往復」は reactor でのみ削減でき、**io_uring では退行した**
ため不採用となった。残るのは**ストリーム状態初期化の固定費**で、これは両バックエンド共通に削れる。

`h2_spawn_for_request` は **1 リクエスト（= 1 ストリーム）ごと**に:

| 箇所 | コスト |
|---|---|
| `connection_metric.set_host(host_str.to_string())` | `String` 確保。`set_host` は接続で最初の 1 回しか値を使わず、**`metrics` feature 無効時ですら `to_string()` は実行**されていた |
| `client_ip: Box::from(client_ip)` | 接続内で毎回同じ値の `Box<str>` 確保 |
| `h2_client_socket_addr(client_ip)` | リクエストごとに文字列 → `SocketAddr` パース（`h2_route_streaming_plan` / `h2_dispatch` / `h2_serve_streaming` で重複実行） |

## 改修内容

- `ActiveConnectionMetric::set_host` を `&str` 受けに変更（確保は「metrics 有効かつ接続の初回」だけ）。
- `H2RequestCtx::client_ip` を `Box<str>` → `Rc<str>` にし、接続あたり 1 回だけ作って `Rc::clone` で配る。
- クライアント `SocketAddr` を接続確立時に 1 回だけ解決し、`Copy` で持ち回る（パース失敗時の
  フォールバック挙動は現状維持）。

設計にあった「チャネル `Rc` の再利用プール」は **1〜3 の効果を測ってから判断する**方針とし、
今回は実装していない（1〜3 の効果が固定費側で +0.89% に留まったため、プール化の追加リスクに
見合わないと判断）。

## 計測（交互 A/B、Linux x86_64 4 コア quiet host、io_uring 既定ビルド）

| 構成 | base 中央値 | new 中央値 | 差 | new 勝ち |
|---|---|---|---|---|
| h2c_file（3B、固定費支配） | 103,349 rps | 104,274 rps | **+0.89%** | 6/8 |
| h2c_proxy（54,576B、18 ラウンド） | 11,070.9 rps | 11,038.7 rps | -0.29%（sd 1.2〜1.4%） | 7/18 |

削ったのが「リクエストごとの小さな確保 2〜3 個」である以上、固定費が支配的な 3B でだけ効き、
バイト単価が支配的な 54KB では出ない、という理論どおりの結果。54KB 側はノイズ内で非退行。
