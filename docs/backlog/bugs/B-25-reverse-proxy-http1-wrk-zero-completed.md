# B-25: 逆プロキシ HTTP/1.1 が高並行 wrk 計測で「完了リクエスト 0」（要調査）

- **優先度**: P3
- **状態**: 調査中（未確定・E2E 低並行では正常）
- **検出**: `tools/perf`（full features ベンチ拡充、2026-07-06）の `h2_1_feat_proxy` 構成

## 事象

`tools/perf/run_perf.sh` の逆プロキシ構成 `h2_1_feat_proxy`（veil が上流 nginx へ
ストリーミング中継）に対し、HTTP/1.1 の `wrk -t4 -c100 -d8s` が
**「0 requests in 8.08s, 3.90MB read」**＝完了リクエスト 0 を計上する（データ転送
自体は約 3.9MB 発生している）。glibc / musl 双方で再現。

一方、次のケースは正常に計測できている:
- HTTP/2（`h2load`）の同一逆プロキシ構成: glibc 718 / musl 863 req/s。
- `h2_1_feat_buffering`（逆プロキシ + full バッファ = Content-Length 応答）: HTTP/1.1 1575 req/s。
- 静的配信（File）系の全構成。

## 推定

- full バッファ（Content-Length 明示）の逆プロキシは wrk が正しく完了計上する一方、
  **ストリーミング逆プロキシ HTTP/1.1** 応答のフレーミング（chunked / connection 挙動）が
  100 並行 keep-alive 下で wrk のリクエスト完了判定に載らない可能性。
- 純粋な wrk のカウント仕様による計測アーティファクトの可能性もある（データは転送済み）。

## 影響と正当性

- **機能正当性は E2E（`tests/e2e_tests.rs`）で網羅的に検証済み**で、逆プロキシ HTTP/1.1 の
  各テストは通過している（低〜中並行）。実運用の破綻を示すものではない。

## 次アクション（未実施）

- ストリーミング逆プロキシ HTTP/1.1 応答を高並行（100 conn）で生パケットキャプチャし、
  Content-Length / Transfer-Encoding / Connection ヘッダと接続クローズ挙動を確認する。
- 併せて `h2load`（HTTP/1.1 非対応）以外の HTTP/1.1 ベンチツール（例: `oha` / `bombardier`）で
  クロスチェックし、wrk 固有のカウント仕様かプロキシ側の応答フレーミング問題かを切り分ける。
