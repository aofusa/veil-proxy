# B-31: レートリミットが thread_local でワーカーごとに分散

## 事象

`rate_limit_requests_per_min = 5` かつ単一クライアント IP から 60 リクエストを送ると、
**200=5, 429=55** のように極端に偏る（4 ワーカーなら理論上 IP あたり最大 20 req/min まで許容されうる）。

## 再現

container_security `rate_limit_probe`（閾値 5、バースト 60）で観測。
初回は 429=0、閾値・バースト調整後に上記パターンが安定再現。

## 影響

- 設定した「分あたり N リクエスト」が **グローバルな IP 制限** として機能せず、
  ワーカー数に比例して実効上限が増える（DoS 緩和効果が期待より弱い）。

## 調査メモ

- `src/config.rs`: `RATE_LIMITER` が `thread_local!` で各 io_uring ワーカーに独立。
- `check_rate_limit(client_ip, limit)` は同一 IP でもワーカー間で状態を共有しない。

## 改修案

- 全ワーカー共有のロックフリーカウンタ（例: シャーディングされた atomic / dashmap）へ移行。
- またはドキュメントで「per-worker 上限」と明記し設定値の解釈を変更（非推奨）。

## 関連

- F-90 rate_limit_probe
- `rate-limit` feature

## 対応状況（保留 — 意図的設計）

thread-per-core モデルではレートリミット状態を **ワーカーごとに独立** させるのが意図的な設計。
グローバル Mutex への集約（一時的な B-31「修正」）は **却下し `thread_local! { RATE_LIMITER }` を復元** した。

設定値 `rate_limit_requests_per_min` は **接続を処理したワーカーあたり** の上限として解釈する。
F-90 `rate_limit_probe` は 429 発火と 200 許可の両方を検証する（per-worker 分散を許容）。