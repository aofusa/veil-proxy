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

### 残（完全ストリーミング = 大規模再設計）

ボディ全体をメモリに溜めず DATA フレーム単位で下流へ逐次転送する「真のストリーミング」は、
現状の「全受信→転送」フローの再設計（HTTP/2 フロー制御 WINDOW_UPDATE と下流書込の結合、
gRPC trailers・WASM ボディフィルタとの整合）を要するため、HTTP/2/gRPC 経路（B-08 で修正
済み）への回帰リスクが高い。bytes 化でアロケーションとコピーは最小化済みのため、完全
ストリーミングは独立した大規模タスクとして継続する。
