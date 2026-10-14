# F-219: メッシュの L7 のトラフィック制御の不足分

**状態: 未着手（2026-10-14 起票・設計。親 F-215）**

## 目的

Istio の VirtualService・DestinationRule・Gateway API の HTTPRoute / GRPCRoute、Linkerd の HTTPRoute・ServiceProfile が
表す L7 の制御を、メッシュモデル（F-218）のルート・クラスタとして実装する。

## 今あるものと不足

| 機能 | Istio / Linkerd での表現 | veil の今 | 不足 |
|---|---|---|---|
| ホスト・パス・ヘッダ・メソッド・クエリの一致 | match | ○ `route.conditions` | 正規表現・前方一致・完全一致の区別の揃え、gRPC のサービス・メソッドの一致 |
| 重み付きの分割（カナリア） | `route[].weight`・backendRefs の weight | △ サーバの重み（LB） | **ルート単位で複数クラスタへの重み付き分割** |
| リトライ | `retries`（回数・条件・per-try タイムアウト）、Linkerd の retry budget | △ 冪等な要求の 1 回の再送（F-177）と resilience | 条件（5xx・reset・connect-failure・gRPC のステータス）・回数・per-try タイムアウト・バジェット |
| タイムアウト | `timeout`、Linkerd の `timeouts.request` | △ 上流の `timeout_secs` | ルート単位の要求全体のタイムアウト |
| ミラー（シャドー） | `mirror`・`mirrorPercentage`、HTTPRoute の RequestMirror | × | 応答を待たない複製の送信（本文の複製はプールから） |
| フォルト注入 | `fault.delay`・`fault.abort` | × | 遅延・中断（割合） |
| ヘッダ操作 | `headers.request/response.set/add/remove` | ○ `add_request_headers` 等 | set と add の区別、応答ヘッダの削除 |
| リダイレクト・書き換え | `redirect`・`rewrite`（URI・authority） | △ | authority の書き換え、正規表現の書き換え |
| 直接応答 | `directResponse` | △ | ルートで固定応答 |
| CORS | `corsPolicy` | × | プリフライトの応答 |
| LB | `loadBalancer`（ROUND_ROBIN・LEAST_REQUEST・RANDOM・一貫性ハッシュ（ヘッダ・Cookie・送信元 IP）） | ○ 一部 | LEAST_REQUEST（P2C）、ハッシュ鍵の種類、ロカリティ（F-218） |
| 接続プール | `connectionPool`（最大接続・最大要求・最大保留・アイドル） | △ | 上限の種類を揃える |
| 外れ値検出・サーキットブレーカ | `outlierDetection`、Linkerd の failure accrual | ○ resilience | 連続 5xx・連続ゲートウェイエラー・排出の割合の上限の揃え |
| HTTP/1 → HTTP/2 の昇格 | Istio の `h2UpgradePolicy`、Linkerd は常に | × | 上流だけ HTTP/2 にする（Linkerd の互換は F-223） |
| サブセット | DestinationRule の subsets（ラベルでエンドポイントを絞る） | × | エンドポイントのラベルでのクラスタの派生（F-218） |

## 設計

- 各機能はメッシュモデルの `RouteAction`・`ClusterPolicy` のフィールドとして持ち、ロード時に解決する（`config::load_backend` と
  同じ原則: ルート単位の派生値は前もって `Arc` で）。メッシュ無しの TOML のルートにも同じ機能を出す（`docs/guide/routing.md`）。
- 重み付き分割は、要求ごとに乱数 1 個（スレッドローカルの xorshift）と累積重みの二分探索。アロケーション無し。
- リトライは本文を再送できるときだけ（F-177 と同じ: 要求バッファのプールから複製。ストリーミングの本文は既定でリトライ
  しない。Envoy も本文のバッファ上限を超えたらリトライしない）。
- ミラーは応答を待たず、失敗は無視してメトリクスにだけ数える。ミラー先の遅延が本線に影響しないよう、ミラーの同時実行数に
  上限を持つ。
- 1 機能 1 コミットで進め、各機能の追加で既存の経路（設定していないとき）のコストが増えないことを確かめる（`tools/perf` の
  交互 A/B）。

## 受け入れ条件

- （必須、F-215）既存のプロキシの性能が退行しない（`tools/perf/suite_ab.sh` の main との交互 A/B、比の中央値 0.98 以上・0.95 未満なし）。
  コンテナで試すときのイメージは musl 版。

- 表の不足を全部埋め、各機能の単体・E2E（`tests/e2e_tests.rs`）。Istio の公式タスク（Request Routing・Fault Injection・
  Traffic Shifting・Request Timeouts・Circuit Breaking・Mirroring）と Linkerd のタスク（Traffic Split・Retries・Timeouts）を
  F-225 の kind の試験で通す。
- `docs/guide/routing.md`・`load-balancing.md`（英日）・`examples/config.toml`。

## 関連

F-215、F-218、F-177（再送）、`src/resilience.rs`。
