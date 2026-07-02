# B-12: HTTP/3 リクエストボディストリーミングが間欠的にストールする

- **優先度**: P3（ユーザー判断で優先度を最下位へ変更。2026-07-02）
- **対応状況**: 進行中（部分修正済み・根本原因の残りは未特定）
- **発見**: 2026-07-02、features full E2E 実行中

## 事象

- E2E `test_http3_request_body_streaming`（HTTP/3 でリクエストボディを逐次アップロードする F-32 経路の検証）が間欠的に ntest タイムアウト（60 秒）で失敗する。
- 単独実行でも再現あり（あるセッションで 3 回中 2 回失敗）。成功時も 1 秒〜32 秒と大きくばらつく。
- 失敗はテストプロセス側のタイムアウトで、パニックメッセージは出ない（ハング）。

## 影響

- HTTP/3 のリクエストボディ逐次転送（アップロード）が実運用でも間欠的に遅延・ストールする可能性。

## 調査メモ

- F-32 第 4 段（HTTP/3 アクターモデル: メインループ = QUIC/H3 ⇔ バックエンドタスクを Rc チャネル + Notify で接続）で実装された経路。
- 環境負荷（4 コア + co-tenant）でのフレーキーの可能性もあるが、ばらつき幅が大きく、Notify の取りこぼし・バックプレッシャのデッドロック・quiche のフロー制御ウィンドウ枯渇などの実バグの可能性を排除できない。
- 関連: [B-11](B-11-expect-100-continue-intermittent-hang.md)（HTTP/1.1 の 100-continue 間欠ハング。別経路だが「ボディ受信の間欠ストール」という症状が類似）。

## 実施済みの修正（2026-07-02）

- `src/http3_stream.rs::run_backend_task` が「リクエストボディ全送信 → レスポンス受信」の**逐次**処理だったため、リクエスト完了前にレスポンスを返すバックエンド（エコー等）で双方向 TCP バッファ満杯 → req チャネル満杯 → QUIC フロー制御でクライアント停止、の双方向デッドロックが成立し得た。アップロードとレスポンス受信を `select_biased!` + fuse で**並行化**（L4 の bidirectional_forward と同方式）して当該デッドロック class を解消。
- ただし修正後も失敗率 ~40%（10 回中 4 回、各試行 30 秒ストール）で残存。**別の根本原因が残っている**（`req_readable` エッジトリガの取りこぼし、quiche のクレジット付与タイミング、またはクライアント（h3+quinn）側の可能性）。

## 再現手順

```bash
./tests/e2e_setup.sh start
for i in $(seq 1 10); do cargo test --test e2e_tests --features full -- test_http3_request_body_streaming; done
```

ハング時のプロキシログ特徴: `Headers: stream_id=0, more_frames=true` の後、一切のログなしに 30 秒後 `Connection closed (timeout)`（QUIC アイドルタイムアウト）。

## 改修案

- quiche のフロー制御（stream/connection window）と `Notify` の待機条件をログで可視化し、ストール地点（クライアント→プロキシ受信 or プロキシ→バックエンド送信）を特定する。
- `select_biased!` の分岐で readable 通知が失われるケース（通知消費とデータ残存の競合）を確認する。

## 受け入れ条件

- `test_http3_request_body_streaming` を 20 回連続実行して全て数秒以内に成功する。
