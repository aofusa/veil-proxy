# F-74: HTTP/2 送信ホットパスのフレーム連結（HEADERS/DATA コアレッシング）

親: [F-73](F-73-http2-send-zerocopy-writeall.md)（HTTP/2 送信ゼロコピー系）の続き。

## 目的

nginx 超えに向けたボトルネック調査で、Veil の **HTTP/2 スループットが最大のギャップ**
であることを確認済み（F-73 で per-frame の二重コピーを排除し +11.6%）。本チケットでは
残る **送信システムコール回数** に着目する。

F-73 完了後も、1 レスポンス送出は次のように **フレームごとに個別の `write_all`
（= 個別の io_uring 送信）** を発行していた:

- `send_response`: HEADERS を 1 回 `write_all` → ボディを `send_data` で DATA ごとに
  `write_all`。→ 最小構成の応答でも **2 回以上の書き込み**。
- ストリーミング CL 経路（`stream_h2_response_body_cl`）: `send_headers` で HEADERS を
  即書き込み → 続く DATA を別書き込み。

HEADERS と最初の DATA が別々の書き込みに分かれると、システムコール回数が増えるだけでなく、
TCP レイヤで HEADERS だけが先行送出されて往復のオーバーヘッドを生む。高性能 HTTP/2 実装
（nginx 等）は 1 レスポンス分のフレームを 1 本の連続バッファへ連結し **1 回で書き込む**。

## 改修内容

1 レスポンス分の HEADERS/DATA/トレイラーを **接続ごとの再利用連結バッファ `write_buf`**
へ積み、境界で **1 回の書き込み** にまとめる（フレームコアレッシング）。

- **`src/http2/frame/encoder.rs`**: 既存バッファへ追記する `encode_headers_into` /
  `encode_data_into`（ゼロ追加確保）を追加。返り値 `Vec` 版はこれを呼ぶ薄いラッパへ集約
  （フレーム内容は不変であることを単体テストで保証）。
- **`src/http2/connection.rs`**:
  - 再利用 `write_buf: Vec<u8>` を追加。スレッドローカルプール（読み込みバッファ F-34 と
    同様）で **接続をまたいで再利用**し、接続ごとの確保・解放を排除。
  - `flush_write_buf()`（連結バッファを 1 回書き込み・容量再利用）を追加。
  - `send_response` は HEADERS + 全 DATA を連結して 1 回で送出。空ボディは HEADERS のみ 1 回。
  - `send_data` は各 DATA を連結バッファへ追記し末尾で 1 回フラッシュ。ウィンドウ枯渇待ちに
    入る前と、大ボディで **128KB 閾値**を超えたときに途中フラッシュ（メモリ上限 + パイプライン化）。
  - ストリーミング応答向けに `send_headers_buffered`（HEADERS を積むだけでフラッシュしない）を
    公開。直後の `send_data` と 1 回の書き込みに連結する。
  - 呼び出し境界では `write_buf` は常に空（`debug_assert`）。直接 `write_all` する制御
    フレーム（SETTINGS/PING/WINDOW_UPDATE/RST_STREAM/GOAWAY）が連結バッファを追い越さない
    不変条件を維持。
- **`src/proxy.rs`**: ストリーミング CL 経路（`stream_h2_response_body_cl`）で、既読ボディ
  断片があるとき `send_headers_buffered` + `send_data` により **HEADERS と最初の DATA を
  1 回の書き込みに連結**。既読断片が無い場合は後続 read で RST を挟む可能性があるため
  HEADERS を即送出して不変条件を守る。

いずれもデータプレーンのゼロコピー・非同期・アロケーション抑制（AGENTS.md ホットパス
絶対規則）を維持する。連結バッファは per-frame Vec 確保も同時に排除する（F-73 の delegate
は残しつつ、複数フレームを 1 確保にまとめる）。

## テスト

- **単体（encoder）**: `encode_headers_into` / `encode_data_into` が返り値 Vec 版とバイト一致。
  既存バッファへの追記でも前置データが保持されること。
- **単体（connection・モックストリーム）**:
  - `send_response` の HEADERS+DATA が **1 回の write** に連結されフレーム整合（END_STREAM/
    END_HEADERS 位置・ボディ一致）が正しいこと。
  - 空ボディが HEADERS 1 フレーム（END_STREAM 付き）1 write になること。
  - `send_headers_buffered` + `send_data` が 1 write に連結されること。
  - `max_frame_size` 分割時も 1 write 内で複数 DATA に分かれ、END_STREAM は最終 DATA のみ、
    連結すると元ボディに一致すること。
  - 送信後に `write_buf` が空へ戻り容量が再利用されること。
- **E2E（features full）**: `test_http2_multiplexed_coalesced_responses` — 同一接続で 8
  ストリームの可変長ボディを echo し全レスポンスがバイト一致（多重化下で連結バッファの
  接続内再利用・途中フラッシュがフレーム境界/ストリーム対応/データを壊さないこと）。
  既存の chunked/CL/小ボディ streaming E2E と gRPC E2E も回帰確認。

## 受け入れ条件

- 小〜中サイズの HTTP/2 応答（初期ウィンドウ内）が **1 回の書き込み** で送出されること
  （単体テストで観測）。
- HTTP/2 統合/E2E・gRPC E2E がグリーンで、フレーミング健全性・ボディバイト一致を維持。
- 全 feature 組み合わせで警告なしビルド、clippy 新規警告なし、`#[allow(dead_code)]` 不使用。

## 状況

完了。単体 660 / http2 E2E 11 / gRPC E2E 35 グリーン。実効スループットの再計測は quiet host
での Docker A/B が必要（[[project_perf_beat_nginx]] の計測手順）。

## 残件

- `src/http2/client.rs`（proxy→バックエンド方向の HTTP/2 送信）は F-73 残件のまま。連結・
  ゼロコピー化は別途。
- `writev`/`sendmsg` scatter-gather（[F-59](F-59-writev-scatter-gather-cache.md) 系）は
  io_uring サーフェス拡大とのトレードオフのため引き続き別評価。今回の単一連続バッファ連結は
  scatter-gather 無しでシステムコール削減を達成する中間解。
