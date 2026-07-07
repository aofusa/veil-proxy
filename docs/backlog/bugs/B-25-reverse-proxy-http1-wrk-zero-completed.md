# B-25: 逆プロキシ HTTP/1.1 が高並行 wrk 計測で「完了リクエスト 0」

- **優先度**: P1（当初 P3。原因が kTLS splice 転送の応答不完全＝実運用のハングと判明したため引き上げ）
- **状態**: 完了（2026-07-07 修正・検証済み）
- **検出**: `tools/perf`（full features ベンチ拡充、2026-07-06）の `h2_1_feat_proxy` 構成

## 事象

`tools/perf/run_perf.sh` の逆プロキシ構成 `h2_1_feat_proxy`（veil が上流 nginx へ
ストリーミング中継、`ktls_enabled = true`）に対し、HTTP/1.1 の `wrk -t4 -c100 -d10s` が
**「0 requests in 10.09s, 3.91MB read」**＝完了リクエスト 0 を計上する（データ転送
自体は約 3.9MB ≒ 100 接続 × 約 40KB 発生している）。glibc / musl 双方で再現。

一方、次のケースは正常: HTTP/2（h2load）逆プロキシ、`feat_buffering`（full バッファ）、
静的配信（File）系の全構成、および **`ktls_enabled = false` の HTTP/1.1 逆プロキシ**。

## 調査（2026-07-07・原因確定）

単一リクエストの再現（curl `--http1.1`）で決定的に切り分けた:

- `ktls_enabled = false`: `Content-Length: 54576` を **0.3 秒で全量受信**（正常）。
- `ktls_enabled = true`: 応答ヘッダーと **40719 バイトのボディ**を受信後、残り 13857
  バイトが永遠に届かず、アイドルタイムアウトで切断（curl exit 56）。veil の CPU は
  0.1%（busy-loop ではない）。警告ログなし＝veil 自身は「全量送信完了」と認識している。

受信済み 40719 バイト = 初期ヘッダー書き込みに同梱されたボディ先頭（約 8KB）+
**ちょうど 16KiB の TLS レコード 2 枚（32768 バイト）**。つまり満杯レコードだけが
送信され、**16KiB 未満の最終部分レコードがカーネル内に保留されたまま**だった。

### 根本原因

`src/runtime/splice.rs` の `SpliceFuture` が **全ての splice に `SPLICE_F_MORE` を
無条件で付与**していた。`fd_out` が kTLS ソケットの場合、`SPLICE_F_MORE` は
`MSG_MORE` として作用し、カーネル TLS（tls_sw）は「後続データがある」ものとして
TLS レコードを開いたまま保持する。転送最終チャンクにも付与されるため、16KiB に
満たない最終レコードは**永遠にフラッシュされない**。

- veil 側は splice の戻り値上「全量転送成功」となるため、エラー・タイムアウト経路
  （B-17 の各種ガード）に一切かからず、keep-alive の次リクエスト待ちへ進む。
- クライアントは Content-Length 未達のまま応答完了を待ち続ける → wrk は 1 リクエスト
  も完了計上できない（「0 requests / 3.9MB read」の正体）。
- `feat_buffering` が正常だったのは full バッファ経路が splice を使わないため。
  HTTP/2 逆プロキシ・静的配信（sendfile 経路）も splice 非使用のため無影響。
- 平文 TCP への splice（L4 パススルー・リクエストボディ方向）でも `MSG_MORE` により
  サブ MSS の末尾セグメントが保留され得る（潜在的なストール要因）。

## 修正内容（2026-07-07）

`src/runtime/splice.rs` / `src/proxy.rs`:

- `splice()` から `SPLICE_F_MORE` を除去（既定は `SPLICE_F_MOVE | SPLICE_F_NONBLOCK`）。
- 「同一 fd_out へ送るデータが確実に続く」中間チャンク専用の `splice_more()` を新設
  （kTLS がレコードを満杯まで詰められるようにする性能ヒントとして温存）。
- `proxy.rs::splice_body_transfer` の pipe→dst 段は `more_follows = remaining > n`
  （現チャンクの後に転送残があるか）で `splice_more` / `splice` を使い分け。
  最終チャンクは必ず `SPLICE_F_MORE` なしで発行し、部分レコードを即時フラッシュする。
- L4 パススルー（`src/l4/proxy.rs`）は境界を事前に知り得ない中継のため、既定の
  `splice()`（MORE なし）を使用（従来の無条件 MORE による潜在ストールも解消）。

## 検証

- curl `--http1.1` + `ktls_enabled = true`: 54576 バイト全量を正常受信。
- `tools/perf` の `h2_1_feat_proxy`（wrk -t4 -c100 -d10s）: 「0 requests」が解消し
  req/s が正常計上されること（修正後ベンチ結果は `docs/perf/` を参照）。
- 既存 E2E の splice リグレッションガード（`tests/e2e_tests.rs` の splice 化検証）通過。
