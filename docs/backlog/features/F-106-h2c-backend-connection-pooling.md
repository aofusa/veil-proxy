# F-106: H2C バックエンド接続プーリング（gRPC 中継の性能改善）

## 機能説明

`use_h2c` バックエンド（gRPC 中継の代表経路: クライアント→veil(TLS h2)→上流(h2c)）への
中継は、HTTP/2 → HTTP/2 プロキシ（`handle_http2_proxy_h2c`）で行う。

## 現状（改修前）

`handle_http2_proxy_h2c` は **リクエストごとに**:

1. `TcpStream::connect_str(addr)` で新規 TCP 接続を張り、
2. `H2cClient::handshake()`（コネクションプリフェース `PRI * HTTP/2.0...` 送信 + SETTINGS
   往復 + SETTINGS ACK 往復）を実行し、
3. 1 リクエストを送って応答を受け、
4. コネクションを破棄する。

HTTP/1.1 / HTTPS(H1) 経路は B-28 で `HTTP_POOL` / `HTTPS_POOL` による接続再利用を実装済みだが、
**H2C 経路は B-28 の残件として明示的に未対応**（「chunked 応答と H2C バックエンドの再利用は残件」）
のままだった。このためリクエストごとの TCP 3-way ハンドシェイク + h2c ハンドシェイクの往復が
gRPC 中継のレイテンシとスループットを直接圧迫していた（`tools/perf` の `feat_grpc` 構成で
静的 HTTP/2 比 約 1/4）。

## 改修内容

- `src/pool.rs`: **`H2cConnectionPool`（スレッドローカル `H2C_POOL`）** を追加。
  `HttpConnectionPool` と同じアイドルタイムアウト + `max_idle` 上限の checkout/return 方式で
  `H2cClient<TcpStream>` をバックエンド `addr` 単位に再利用する。io_uring `TcpStream` は
  ワーカースレッド ring に紐づくため、thread-per-core のスレッドローカルで同一スレッド内
  再利用のみ行う。
- `src/proxy.rs`: `handle_http2_proxy_h2c` を再構成。
  - 接続取得を **プール優先**（ヒット時は connect + handshake を完全に省略）に変更。
  - プールヒット接続が stale（アイドル中に上流が切断）で初回送信が失敗した場合は、
    **新規接続で 1 回だけリトライ**して要求を落とさない。
  - 応答成功後、ストリーム ID 枯渇前（`H2cClient::is_reusable()`）の健全な接続を
    **プールへ返却**して次リクエストで再利用する。
  - 接続確立/ハンドシェイクを `h2c_connect_and_handshake`、エラー応答を `send_h2c_error`
    へ切り出し。
- `src/http2/client.rs`: **接続再利用で顕在化する送信ウィンドウ枯渇バグを修正**。
  - `H2cClient` はコネクションを跨いで `next_stream_id`（1,3,5,…）を単調増加させ、
    複数リクエストを直列に流せる状態を元々持つ（HPACK 動的テーブル・受信バッファも保持）。
  - しかし従来 `Frame::WindowUpdate` を無視しており、送信のたびに減算した
    `conn_send_window` を回復しなかった。1 リクエスト 1 接続では毎回リセットされ顕在化
    しなかったが、**再利用すると数千リクエストで枯渇し「Send window exhausted」で失敗**する。
    接続レベル（stream 0）の WINDOW_UPDATE を `conn_send_window` へ積み増す
    `apply_window_update` を追加し、両受信ループ（`receive_response` /
    `receive_grpc_response`）で反映するよう修正。
  - 再利用可否判定 `is_reusable()`（HTTP/2 31bit ストリーム ID 上限手前で切替）を追加。

## 受け入れ条件

- gRPC 中継（`feat_grpc`）が接続再利用で機能維持（Non-2xx=0）かつスループット改善。
- 既存 gRPC / HTTP/2 E2E が回帰しない。
- HTTP/1.1・HTTP/2 の静的配信・逆プロキシ性能が劣化しない（H2C 経路のみの変更）。
- 単体テスト: h2c クライアントの直列再利用（stream id 1→3）と WINDOW_UPDATE による
  送信ウィンドウ回復を検証。

## 依存・リスク

- 再利用接続の HPACK 動的テーブル・受信バッファはコネクションスコープで一貫するため
  正当（HTTP/2 仕様通り）。
- stale 接続はアイドルタイムアウトで無効化 + 初回失敗時の新規接続リトライで吸収。
- io_uring TcpStream のスレッド親和性のため、プールはスレッドローカル（既存 `HTTP_POOL`
  と同一方針）。
