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

## 対応状況: HTTP/2 レスポンス方向（content-length + chunked）+ リクエスト方向ストリーミング完了。HTTP/3 は残

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
- **gRPC trailers / WASM ボディフィルタ**: ボディ全体を要求するため従来経路（影響なし）。
- ※ **chunked** は第2フェーズで逐次デコードストリーミング化済み（下記参照）。

### 検証

- `cargo test --bins --features full` 585 通過、`cargo test --test integration_tests --features full`
  53/53 通過（既存フレーキー `cache::revalidation::test_active_count` のみ、本変更と無関係）。

## 対応状況（全面書換・第2フェーズ）: HTTP/2 chunked レスポンスの逐次デコードストリーミング

第1フェーズでバッファ経路へ退避していた **chunked レスポンス** を、**逐次デコードしながら
DATA フレームへストリーミング転送**する経路に置き換えた。従来は `full_body: Vec<u8>` に
全溜め → `decode_chunked_body` で再アロケートしていた二重確保を排除する。

### 実装（`src/http_utils.rs` + `src/proxy.rs`）

- 新規 `ChunkedDecoder::next_data_span()`（+ `ChunkedSpan`）: **ゼロコピー span 抽出 API**。
  フレーミング（チャンクサイズ・CRLF・トレーラー）を内部ステートマシンで消費し、
  `ReadingChunkData` に入った時点で入力スライス内のデータ run を **1 回の計算で確定**して
  返す（バイト単位ループや中間バッファなし）。返した範囲は入力のサブスライスなので、
  呼び出し側は中間 `Vec` を持たずに下流へそのまま送出できる。
- 新規 `stream_h2_response_body_chunked()` / `h2_drain_chunked_spans()`: 各チャンクの
  データ範囲（読み取りバッファのサブスライス）を `send_data` で逐次送出。各 `send_data` は
  HTTP/2 フロー制御（conn/stream ウィンドウ + WINDOW_UPDATE 待ち）に従うためバックプレッシャが
  効き、RSS をペイロードに比例させない。終端チャンク検出時に 0 長 DATA + END_STREAM で閉じる
  （トレーラーはボディに含めない）。バックエンドが終端前に切断した場合も END_STREAM で安全に閉じる。
- `handle_http2_proxy_http` / `_https` に **chunked ストリーミング分岐**を追加: 非圧縮 + chunked の
  場合にストリーミング、圧縮時は従来バッファ経路（回帰なし）。`content-length`/`transfer-encoding`
  ヘッダは HTTP/2 へ転送しない。

### 検証（第2フェーズ）

- 単体: `next_data_span` のテスト 10 件（分割境界・トレーラー・チャンク拡張・サイズ制限・
  `decode_chunked_body` との一致を全分割点で検証）。`cargo test --bins --features full` 597 通過。
- 結合: `cargo test --test integration_tests --features full` 53/53 通過。
- E2E: `test_http2_chunked_response_streaming`（実 h2 クライアント）追加。`Transfer-Encoding:
  chunked` で 200,000 バイト（初期フロー制御ウィンドウ 65,535 超）の決定論的ボディを
  バイト単位まで完全一致で再構成（ストリーミング・チャンクデコード・END_STREAM・
  バックプレッシャの End-to-End 保証）。`features full` E2E 389 通過 / 既知の負荷フレーキー
  3 件（stress・431・100-continue。いずれも本変更と無関係でタイムアウト依存、isolation で通過）。

## 対応状況（全面書換・第3フェーズ）: HTTP/2 リクエスト方向の真のストリーミング

レスポンス方向（第1・2フェーズ）に続き、F-32 doc 推奨の段階導入に従って **HTTP/2 リクエスト
ボディの全バッファリングを排除**するアップロードストリーミングを実装した。「全受信→処理」
シーケンシャルモデルを、**HEADERS 受信時点でバックエンド接続を開始し DATA フレームを逐次転送**
する設計へ再構築した。

### 実装（`src/http2/{stream,connection}.rs` + `src/proxy.rs`）

- **連鎖の起点**: `handle_headers`/`handle_continuation` が、HEADERS（+CONTINUATION）完了かつ
  END_STREAM 未受信（ボディ継続）の時点で `ProcessedRequest{body_pending:true}` を返す。
  proxy ループはこのシグナルでストリーミング適格性を判定する（非適格なら無視され、DATA は
  従来どおり `request_body` に蓄積 → END_STREAM でバッファ処理にフォールバック。回帰なし）。
- **ゼロコピー転送**: `Stream::recv_data` を **accounting**（フロー制御ウィンドウ消費・受信量
  カウント・content-length 検証・状態遷移）と **バッファリング** に分離。新規
  `Stream::recv_data_accounting` / `Http2Connection::recv_data_for_streaming` はアカウンティング
  のみ行い `request_body` へコピーしない。proxy は受信した DATA フレームの所有バッファを
  `Bytes::from(data)`（アロケーション再利用・ゼロコピー）で取り出し、`send_backend_chunk` で
  **ペイロードをゼロコピーのまま** chunked エンコードしてバックエンドへ送出する（チャンクサイズ
  行と CRLF のみ小バッファ）。
- **バックプレッシャ**: 各 DATA を `send_backend_chunk` で**書き込み完了まで次フレームを読まない**。
  これによりクライアント → プロキシ → バックエンドの TCP バックプレッシャが自然に伝播し、
  プロセスヒープ保持は最大 1 フレーム分（RSS がペイロードサイズに比例しない）。WINDOW_UPDATE は
  受信消化に連動して送出される。
- **エントリ**（`handle_h2_request_streaming`）: ルーティング → 適格性判定 → バックエンド接続
  （HTTP/HTTPS、`relay_h2_response` で応答リレー）→ ログ → 転送中に完了した他ストリームの遅延処理。
  応答リレーは第1・2フェーズの `relay_h2_response`（総称化済み）を再利用する。
- **適格性**: Proxy バックエンド + HTTP/1.1（h2c 以外）+ バッファリング `streaming`/`adaptive`
  （`full` 以外）+ WASM モジュール非適用 + 非 gRPC + セキュリティ（IP/メソッド/レート）許可。
  ボディサイズ上限（`max_request_body_size`）は転送中に強制し、超過時は 413 + RST_STREAM。

### 付随修正（TLS）: 受信平文バッファ上限超過

大容量 h2/TLS アップロードの End-to-End 検証中に、rustls の `received_plaintext`（既定 16KB 上限）
が溢れて `read_tls` が `received plaintext buffer full` を返し接続が切断される**既存バグ**を発見・修正。
`ktls_rustls.rs` / `simple_tls.rs` の read 実装を、`process_new_packets` 後に毎回平文を退避バッファへ
ドレインしてから残りの生 TLS を投入する方式へ変更（received_plaintext を常に空に保つ）。kTLS
（カーネル）モードは本経路を通らず影響なし。これにより大容量アップロードがバッファ/ストリーミング
双方で正しく読めるようになった。

### 検証（第3フェーズ）

- 単体: `recv_data_accounting`（フロー制御/content-length 検証/非バッファ）、`push_chunk_size_line`
  （16 進エンコードの標準ライブラリ一致）。`cargo test --bins --features full` 601 通過。
- 結合: `cargo test --test integration_tests --features full` 53/53 通過。
- E2E: echo バックエンド（chunked リクエストボディをデコードして往復）を追加。
  `test_http2_request_body_streaming`（200,000 バイト、初期フロー制御ウィンドウ 65,535 超）の
  **往復バイト単位完全一致**でストリーミング・chunked エンコード/デコード・終端・フロー制御・
  バックプレッシャの End-to-End を保証。`test_http2_request_body_streaming_small`（単一 DATA 経路）も追加。

### 残（HTTP/3）

- **HTTP/3 レスポンス/リクエストストリーミング**: quiche のパケット処理ループ内でバックエンド
  読み込みと `send_body`/`recv_body` を交互に行う再設計が必要（quiche 自身のフロー制御・
  partial_responses バッファとの結合）。独立タスクとして継続。
