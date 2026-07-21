# F-131: HTTP/2 メインループファストパス + Stream オブジェクト再利用プール

## 背景

ユーザー提案「HTTP/2 の直列パース + 過剰タスク間通信による CPU 飽和を解消し HTTP/3 並みのスループットへ」に基づく4項目の改善案のうち、調査の結果 **フレームパース/Waker のバッチ化（F-116 で実装済み）** と **HPACK ハフマン LUT 化（F-121 で 4-bit 実装済み・8-bit 拡張は投資対効果が低く見送り）** は対応不要と判断。本チケットは残る2項目を実装する。

## 改修内容

### A. HTTP/2 メインループ・インラインファストパス

`h2_spawn_for_request`（`proxy.rs`）が全リクエストを無条件で `TaskPool::spawn` + チャネル（`H2RespMsg` Sender/Receiver）経由にディスパッチしていたのを改め、バックエンド I/O・WASM 実行を伴わない同期完結の応答（Prometheus メトリクス・管理 API・セキュリティチェック拒否・404 Not Found・`Backend::Redirect`〔WASM 非適用時のみ〕）はメインループから `conn.send_headers_buffered_end()`/`conn.queue_data_frames()` を直接呼んでインライン応答する。タスク生成・チャネル・`Notify` 起床のオーバーヘッドをゼロにする。

### B. Stream オブジェクト再利用プール（free-list）

`StreamManager`（`src/http2/stream.rs`）に free-list を追加し、クローズ済み `Stream` の `Vec`/`BytesMut` バッファを `clear()`（容量保持）して再利用する。`HashMap<u32, Stream>` のキー管理自体は維持（ID→スロットの直接マッピングは、生成順とクローズ順が一致しない HTTP/2 の性質上、単純な mod 容量方式では同時アクティブ数以下でも衝突し得るため採用せず、安全性を優先）。接続チャーン時のヒープ再確保回数を削減する。

## 設計ドキュメント

`docs/artifacts/f131_http2_fastpath_streampool_design.md`

## 状態

進行中（設計完了・実装中）
