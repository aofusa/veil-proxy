# F-174: 多重化対応の HTTP/2 上流クライアント

**状態: 完了（feat/v080-limitations）**

F-171（HTTP/3 → h2c の全二重）、F-175（上流 HTTPS の h2）、B-106（ストリーム単位のフロー制御）の共通基盤。

## 設計

- `src/http2/upstream_mux.rs`: 上流接続 1 本を所有するアクタータスクと、ストリームごとのハンドル。
  - 送信: 接続・ストリームの両ウィンドウを見て、足りない分はストリームごとに保留。
  - 受信: 下流が消費したぶんだけ WINDOW_UPDATE（F-116 の消費連動補充）。
  - `SETTINGS_MAX_CONCURRENT_STREAMS` を尊重し、満杯なら同じ上流へ 2 本目の接続を張る。
  - GOAWAY 受信後は新規ストリームを割り当てない。
- トランスポートは汎用（平文 TCP / UDS / `ClientTls`）。`:scheme` は TLS なら `https`。
- スレッドローカルのプール（`(接続先, scheme, SNI)` ごと）。既存の `H2C_POOL`（1 接続 1 ストリームの直列利用）を置き換える。
- 本文は `Bytes` のムーブのみ（チャンクごとの確保なし）。

## 結果（2026-10-09）

- `src/http2/upstream_mux.rs`: `H2Mux`（接続ハンドル）・`UpStream`（ストリームハンドル）・アクター `run_actor`。
  - `open` は呼び出し側で同期的に HPACK エンコードして HEADERS を送信キューへ積む（要求ヘッダを所有コピーしない）。
    プリフェース・SETTINGS・接続ウィンドウの WINDOW_UPDATE は開始時にキューの先頭へ積み、サーバの SETTINGS を待たない。
  - アクターはソケット可読（`wait_readable_fd`）・`Notify`（新規ストリーム・キャンセル）・要求本文の到着
    （`Receiver::poll_recv`）・応答チャネルの空き（`Sender::poll_ready`）を 1 つの `poll_fn` で待つ。
  - 受信ウィンドウ: ストリーム 256KB / 接続 4MB。下流のチャネルへ渡したぶんだけ WINDOW_UPDATE する。
  - CONTINUATION の組み立て、1xx の読み捨て、GOAWAY（`last_stream_id` より後は再送可）、REFUSED_STREAM（再送可）、
    PUSH_PROMISE（ENABLE_PUSH=0 なので接続エラー）、ハンドル drop で RST_STREAM(CANCEL)。
  - ストリームが無いまま `idle_connection_timeout_secs` を過ぎると GOAWAY で閉じる（0 ならプールしない）。
- プールはワーカーごと（`H2_MUX_POOL`、`PoolKeyStr` キー）。空きのある接続が無いときだけ新しい接続を張る。
- 旧 `H2cClient`（`src/http2/client.rs`）と `H2C_POOL` を削除。HTTP/2 フロント（`h2_proxy_h2c`）・HTTP/3 のバッファ経路
  （`proxy_to_h2c_backend_async`）・HTTP/1.1 フロント（`proxy_h2c`。従来は要求ごとに接続を張っていた）をすべて置き換えた。
- 単体テスト（socketpair + 同期の模擬サーバ、io_uring / epoll 両方）: 2 ストリームの多重化とストリームウィンドウの遵守、
  REFUSED_STREAM の再送可否、MAX_CONCURRENT_STREAMS、キャンセル時の RST_STREAM(CANCEL)。

計測（交互 A/B、Docker、h2load `-c100 -m10` で grpcbin の unary SayHello、base = 改修直前のコミット）:

| フロント | base | new | 差 |
|---|---|---|---|
| HTTP/2（TLS） | 8,382（3 ラウンド。1 ラウンドは base が応答しなくなり計測不能） | 19,144 | +128% |
| HTTP/3 | 8,181（2 ラウンド。他の 2 ラウンドは base が 1,400 rps・エラーあり） | 21,820 | +167% |

base（直列利用の旧プール）は高並列で上流接続が増え、時折応答しなくなった。
