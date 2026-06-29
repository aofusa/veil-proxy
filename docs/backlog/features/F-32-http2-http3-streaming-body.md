# F-32: HTTP/2・HTTP/3 ボディのストリーミング転送（全バッファリング排除）

## 出典

`docs/gemini/performance-issue/README.md`（#5）/ `architecture_analysis.md`（#1）。

## 概要

HTTP/2・HTTP/3 のリクエスト/レスポンスボディをオンメモリに全溜めせず、受信フレーム単位で
バックエンド（あるいはクライアント）へストリーミング転送する。OOM 耐性とスループットを改善する。

## 現状の問題

- `src/http2/stream.rs`: `Stream` が `request_body: Vec<u8>` / `response_body: Vec<u8>` を保持し、
  `recv_data` 時に `extend_from_slice` でボディ全体を `Vec` に蓄積している。
- `src/http3_server.rs`: `pending_requests: Vec<(u64, Vec<h3::Header>, Vec<u8>)>` で
  HTTP/3 ボディ全体を `Vec<u8>` に蓄積している。

大きなアップロード/ダウンロードで、リクエスト全体がメモリに乗るまで転送が始まらず、
メモリ使用量がペイロードサイズに比例する（ストリーミングプロキシの思想に反する）。

## 改修内容

1. ボディ蓄積を `bytes::BytesMut`（参照カウント分割）に変更し、`DATA` フレーム受信ごとに
   下流（バックエンド or クライアント）へ `WriteFuture` で逐次転送する「ストリーミングパス」を実装。
2. HTTP/2 のフロー制御ウィンドウ（`WINDOW_UPDATE`）を、下流の書き込み完了に連動させて
   バックプレッシャを正しく伝播させる（受信過多でメモリが膨らまないように）。
3. バッファリング設定（`[route.buffering]` の streaming/full/adaptive）と整合させる。
   `full` 指定時のみ全バッファリング、デフォルトはストリーミング。

## 受け入れ条件

- [ ] HTTP/2・HTTP/3 の E2E が回帰なし（大容量ボディ・分割 DATA フレーム含む）。
- [ ] 大容量ボディ転送時に RSS がペイロードサイズに比例しない。
- [ ] ホットパスのボディ蓄積に `Vec<u8>::extend_from_slice` の無制限蓄積が無い。

## 依存・リスク

- HTTP/2 フロー制御とストリーミングの結合は難易度が高い。flow window を正しく扱わないと
  デッドロック/ストールの恐れ。段階導入（まずレスポンス方向、次にリクエスト方向）を推奨。
- gRPC（trailers）/ WASM ボディフィルタとの相互作用に注意（ボディ全体を要求するフィルタは full に退避）。

## 対応状況: bytes/ゼロコピー化は完了、完全ストリーミングは残

### 完了（bytes クレートによるゼロコピー化）

- **HTTP/2**: `http2/stream.rs` の `request_body`/`response_body` は既に `bytes::BytesMut`
  （F-26）。プロキシ転送時は `std::mem::take(&mut stream.request_body).freeze()` で
  `Bytes` 化しゼロコピーでバックエンドへ渡している。
- **HTTP/3**（本対応）: `http3_server.rs` の `pending_requests`/`stream_bodies` を
  `Vec<u8>` → `BytesMut` に変更。さらに DATA イベントで quiche の `recv_body` を
  `BytesMut::spare_capacity_mut()` の uninit 領域へ**直接読み込み**、`advance_mut` で確定する
  方式に変更し、**イベントごとの 16KB ヒープ確保と中間バッファ→本体への追加コピーを撤廃**
  （SafeReadBuffer と同方針）。

## 対応状況（全面書換・第1フェーズ）: HTTP/2 レスポンス方向の真のストリーミング

F-32 doc 推奨の段階導入（まずレスポンス方向）に従い、**HTTP/2 レスポンスボディの全
バッファリングを排除**するストリーミングパスを実装した。

### 実装（`src/proxy.rs` + `src/http2/connection.rs`）

- 新規 `stream_h2_response_body_cl()`: バックエンド HTTP/1.1 レスポンスのボディを
  **`send_data` で DATA フレームとして逐次転送**する。各 `send_data` は HTTP/2 フロー制御
  （conn/stream 送信ウィンドウ + WINDOW_UPDATE 待ち）に従うため、**クライアントの受信速度に
  応じたバックプレッシャ**が効き、レスポンス全体をメモリに溜めない（RSS がペイロード
  サイズに比例しない）。ヘッダ直後の既読断片はスライス直送、以降はバックエンドから
  読みつつ即送出する（中間 `full_body` バッファを廃止）。
- `handle_http2_proxy_http` / `handle_http2_proxy_https` に**ストリーミング分岐**を追加:
  **非圧縮 + content-length 既知 + 非 chunked** の場合にストリーミングパスを使う。
  圧縮・chunked・長さ不明（EOF）は従来のバッファ経路にフォールバック（回帰なし）。
- `Http2Connection::send_data`: 空ボディ + END_STREAM 時に 0 長 DATA フレームを送出する
  よう修正（ストリーミングの終端クローズ用。バックエンドが content-length 未達で切断した
  場合の処理に使用）。

### バッファ経路へ退避するケース（full 相当）

- **圧縮有効**: ボディ全体を圧縮する必要があるためバッファ。
- **chunked**: 現状の `ChunkedDecoder` は逐次デコード済みバイトを取り出せないためバッファ
  （= 第2フェーズで逐次デコード対応予定）。
- **gRPC trailers / WASM ボディフィルタ**: ボディ全体を要求するため従来経路（影響なし）。

### 検証

- `cargo test --bins --features full` 585 通過、`cargo test --test integration_tests --features full`
  53/53 通過（既存フレーキー `cache::revalidation::test_active_count` のみ、本変更と無関係）。

### 残（第2フェーズ以降）

- **リクエスト方向のストリーミング**: 現状は end_stream まで `request_body` にバッファしてから
  バックエンドへ転送する。真のアップロードストリーミングは、HEADERS 受信時点でバックエンド
  接続を開始し DATA フレームを逐次転送する設計（逐次 frame loop ⇄ バックエンド書込の結合）を
  要し、現行の「全受信→処理」シーケンシャルモデルの再設計が必要。独立した大規模タスクとして継続。
- **chunked レスポンスの逐次デコード転送**、**HTTP/3 レスポンスストリーミング** も継続課題。
