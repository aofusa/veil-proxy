# B-11: Expect: 100-continue の POST が間欠的にハングする

- **優先度**: P1
- **対応状況**: 完了（2026-07-02 根本原因特定・修正。`test_100_continue` 20 回連続成功（全て 0.5 秒未満）+ curl 実フロー 60/60 成功）
- **発見**: 2026-07-02、E2E `test_100_continue` のフレーキー調査

## 事象

- E2E `test_100_continue`（HTTPS POST、`Expect: 100-continue`、100 バイトボディ）が間欠的に失敗する。
- テストは 15 秒タイムアウト × 3 リトライ付きだが、失敗時は 3 回とも 15 秒タイムアウトする（46 秒で失敗）。
- 単独実行でも約 1/5 の頻度で再現し、成功時も 0.2 秒〜31 秒と実行時間が大きくばらつく（内部リトライで回復している）。並列実行の干渉ではない。

## 影響

- `Expect: 100-continue` を使うクライアント（curl の大きい POST 等）が間欠的にタイムアウトする可能性。

## 調査メモ

- プロキシ側の 100 Continue 送信は `src/proxy.rs`（`check_expect_continue` → `HTTP_100_CONTINUE` 書き込み、WRITE_TIMEOUT 付き）で、送信自体は問題なさそう。
- クライアント（hyper）は Expect ヘッダーを付けてもボディを即送信する。ヘッダーと同時に読み込まれた先行ボディバイト（`accumulated`）と、100 送信後のボディ読み取り経路の間の競合が疑わしい。
- kTLS は E2E では無効（`ktls_enabled = false`）のため kTLS/splice 経路は無関係。
- F-39 以前から存在するかは未確認（F-39 変更箇所は Content-Length kTLS splice 経路のみで本件とは別経路）。

## 改修案

- 100 Continue 送信後のボディ読み取りで、ヘッダーパース時に `accumulated` に先読み済みのボディを消費しているか確認する。
- プロキシの debug ログ + パケットキャプチャでハング地点（クライアント→プロキシ or プロキシ→バックエンド）を特定する。

## 根本原因と修正（2026-07-02 完了）

curl（RFC どおり 100 受信を待ってからボディを送るクライアント）の verbose トレースで、ハング時に
**クライアントへ「HTTP/1.1 100 Continue」が 2 回届き、2 回目にプロキシの応答ヘッダ修飾
（X-Proxied-By 等）が付いている**ことを確認して特定した。`accumulated` の先読みボディ消費（当初仮説）は問題なかった。

**根本原因**: プロキシは `Expect: 100-continue` に対して自ら 100 Continue を返すのに、
`Expect` ヘッダーを**バックエンドにもそのまま転送**していた。バックエンド（E2E では veil 自身）も
100 Continue 中間応答を返し、それが最終応答（200）と**別の TCP セグメントで先着**した場合、
応答パーサ（`parse_http_response`）に 1xx 中間応答のスキップ処理がないため、**バックエンドの 100 を
最終応答ヘッドとして解析・修飾・転送**してしまう。クライアントは 100 を中間応答として読み飛ばし
最終応答を待ち続け、本物の 200 は転送されずハング。100 と 200 が同一 read で届けば偶然正常動作する
ため間欠的（実測 ~13%）だった。hyper クライアント（ボディ即送信）では 100/200 が同一セグメントに
なりやすく、E2E での再現が確率的だった。

**修正**:

1. **`Expect` ヘッダーをバックエンドへ転送しない**（プロキシが 100 応答責務を負い、ボディを
   無条件転送するため転送は無意味かつ有害）。HTTP/1.1（`proxy.rs` `handle_proxy`）・
   H2C バックエンド（`http2/client.rs`）・HTTP/3 フロント（`http3_server.rs`
   `build_h1_request_head`）の全リクエストビルダーに適用。
2. **バックエンド応答の 1xx 中間応答（101 を除く）を読み捨てる**（RFC 9110 §15.2。
   103 Early Hints 等を返すバックエンドへの一般的堅牢化）。`http_utils.rs::drain_interim_responses`
   を新設し、HTTP/1.1 の応答リーダー 3 箇所（`transfer_response_with_compression` HTTP/HTTPS 両系統・
   `receive_and_buffer_response`・`splice_transfer_response_ktls`）と HTTP/3 ストリーミングの
   `stream_response`（`drain_interim_and_find_header_end`）に適用。

**テスト**: 単体 8 件（`drain_interim_*`）+ E2E `test_100_continue_deferred_body` を新設
（raw TLS で curl と同じ「100 受信後にボディ送信」フローを 30 回連続検証。旧実装の再現率 ~13%/回
に対し見逃し率 ~1.5%）。

## 受け入れ条件

- `test_100_continue` を 20 回連続実行して全て 1 秒未満で成功する。
  → **達成**（2026-07-02、20/20 成功・全て 0.28〜0.45 秒・内部リトライ 0。加えて curl 実フロー
  60/60 成功（修正前 ~13% ハング）、E2E 全スイート 402/403 通過（残 1 は既知の負荷フレーキー
  `test_concurrent_connection_stress`、loadavg 9 の co-tenant 負荷下）、単体 636・統合 53 通過）
