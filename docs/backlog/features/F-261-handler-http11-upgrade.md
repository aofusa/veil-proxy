# F-261: API ライブラリのハンドラで HTTP/1.1 のプロトコルアップグレード（101 Switching Protocols）を扱う

**状態: 未着手（2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)。WebSocket（[F-262](F-262-handler-websocket-rust.md)）の土台）**

## 背景

- ハンドラ（`Backend::Handler`）は「要求 1 個 → 応答 1 個」だけを返せる。接続そのものを引き取る手段が無い。
- `src/proxy/h1.rs` 979〜1066 行: `Upgrade: websocket` の要求は `Backend::Proxy` のときだけ中継し、**それ以外（ハンドラ・File）は
  400 を返して切る**。WebSocket 以外の `Upgrade`（独自プロトコル・`h2c` 等）はハンドラへ渡るが、ハンドラが 101 を返しても
  `dispatch::write_http1_head`（137〜176 行）が `Connection: keep-alive` を足し、接続はそのまま次の要求の読み取りへ戻る。
- HTTP/2・HTTP/3 の経路（`dispatch::h2_h3_headers` 104〜113 行）は `upgrade` を取り除く（RFC 9113 §8.2.2 の接続固有ヘッダ）。

## 制約（今回の依頼）

- **ライブラリ利用時だけの機能**。本体（プロキシ・Web サーバ）のバイナリには 1 行も入らない: 本体側の変更は `h1.rs` の
  `cfg(feature = "handler")` の分岐だけ（AGENTS.md）。`tests/api/library_e2e.sh nonregression` に新しい記号の検査を足す。
- ホットパス規則: アップグレードしない要求の経路に確保・分岐を増やさない（ハンドラの要求で 1 回の `Option` の確認だけ）。

## 設計

### 1. API（Rust、`veil::api::upgrade`、feature `handler`）

```rust
/// 要求がアップグレードを求めているか（`Connection: upgrade` を含み `Upgrade` がある。HTTP/1.1 のみ true）。
impl Request { pub fn upgrade_protocol(&self) -> Option<&[u8]>; }

/// 101 を返し、書き終えた後に接続を `on_upgrade` に渡す応答を作る。
impl Response {
    pub fn switching_protocols<F, Fut>(protocol: &str, on_upgrade: F) -> Response
    where F: FnOnce(Upgraded) -> Fut + 'static, Fut: Future<Output = ()> + 'static;
}

/// アップグレード後の接続（平文・rustls・kTLS のどれでも同じ型）。
pub struct Upgraded { /* ServerTls + 先読みの残り（Bytes） + 接続の終了の通知 */ }
impl Upgraded {
    pub async fn read(&mut self, buf: BytesMut) -> (io::Result<usize>, BytesMut);   // runtime の所有権渡しの I/O と同じ形
    pub async fn write_all(&mut self, buf: Bytes) -> io::Result<()>;
    pub async fn write_vectored_all(&mut self, bufs: &[Bytes]) -> io::Result<()>;    // ヘッダ + 本文を 1 回の writev
    pub async fn shutdown(&mut self) -> io::Result<()>;
    pub fn client_ip(&self) -> &str;
    /// graceful shutdown（F-187）の開始で解決する Future。長く開く接続はこれを select して閉じる。
    pub fn shutdown_signal(&self) -> ShutdownSignal;
}
```

- `veil::tokio::io::{AsyncRead, AsyncWrite}` も実装する（`tokio-compat` のとき）。tokio の生態系のプロトコル実装
  （tokio-tungstenite 等）を載せられるようにするため。
- マクロ: `#[veil::route(path = "/x", method = "GET")]` のハンドラが `Response::switching_protocols` を返すだけ（新しい属性は要らない）。
- 引き取りを拒否する予約済みのプロトコル: `h2c`（本体の h2c の判別と衝突する）・`websocket` を F-262 の API 以外で返すことは許す
  （自前で WebSocket を実装したい人のため）。`h2c` を返したハンドラは 500 にして `warn!`（開発時の誤り）。

### 2. HTTP/1.1 の経路（`src/proxy/h1.rs`、`cfg(feature = "handler")` の分岐だけ）

1. 979 行の `is_websocket` の分岐: `Backend::Handler` のときは 400 にせず、通常のハンドラの経路へ進める
   （`Backend::Proxy` の中継と `Backend::File` 等の 400 は今のまま。`cfg(not(feature = "handler"))` のバイナリは同じコード）。
2. `handle_api_handler`（1511 行）: ハンドラの応答が `Response::switching_protocols` なら:
   - 要求に `Upgrade` が無い・HTTP/1.0・本文付き（`Content-Length > 0` か chunked。RFC 9110 §7.8 では許されるが、本文と
     アップグレード後のバイト列の境界が曖昧になるので断る）→ 400 に置き換える。
   - 101 のヘッダを書く: `HTTP/1.1 101 Switching Protocols`・`Connection: Upgrade`・`Upgrade: <protocol>`・ハンドラが足したヘッダ。
     `write_http1_head` の `Connection: keep-alive`・`Content-Length` を付けない版を使う（1xx の分岐を足す）。
   - **先読みの残り**: 要求の解析で読みすぎたバイト列（ヘッダの直後にクライアントが続けて送ったフレーム）を `Upgraded` の先頭に渡す
     （B-145 の教訓。読み捨てない・101 より先に処理しない）。
   - `ServerTls` を `Upgraded` に入れ、`on_upgrade` の Future をワーカーで `spawn` する。接続のループは `None` を返して抜ける
     （接続は Future の持ち物になる）。`ConnectionGuard`（同時接続数・`max_concurrent_connections`）は Future に移し、
     Future の終わりで解放する（アップグレードした接続も同時接続数に数える）。
   - アクセスログは 101 で 1 行（今のハンドラと同じ）。続く通信の長さは記録しない（後続の候補: 閉じたときの送受信バイト数）。
3. タイムアウト: アップグレード後は HTTP のキープアライブ・読み取りのタイムアウトの外。`[api] upgrade_idle_timeout_secs`
   （既定 0 = 無し）を `Upgraded::read` に効かせる（`runtime::time::timeout`。I/O ごとにカーネルタイマーを張らない B-72 の作りのまま）。
4. graceful shutdown（F-187）: アップグレードした接続は「処理中の要求」ではないので待たない。`shutdown_signal()` を解決し、
   `[server] shutdown_timeout` の後は Future を drop する（接続を閉じる）。

### 3. HTTP/2・HTTP/3

- RFC 8441（拡張 CONNECT。HTTP/2 上の WebSocket）・RFC 9220（HTTP/3）は範囲外。`Request::upgrade_protocol()` は None を返し、
  ハンドラが `switching_protocols` を返したら 400（HTTP/2・3 にアップグレードは無い）。`SETTINGS_ENABLE_CONNECT_PROTOCOL` は送らない
  （ブラウザは HTTP/1.1 で WebSocket を開く）。後続のチケット候補として F-258 に記録する。

### 4. C API・Python

- Python は [F-263](F-263-python-async-runtime-websocket.md)（常駐のイベントループ + `veil.Protocol` で受ける）。
- C: `veil_response_upgrade(resp, protocol, const veil_l4_callbacks_t *cb, void *ud)`。アップグレード後は L4 の TCP ハンドラ
  （F-210 のイベントのコールバック: `on_data`・`on_close`、`veil_transport_write`）と同じ形で受ける（プールのスレッドを長く占有しない）。
  既存の L4 の橋渡しを再利用する。

## 試験

- 単体: 101 のヘッダの書式（`Connection`・`Content-Length` を付けない）、本文付きの要求・HTTP/1.0・`Upgrade` 無しで 400、
  `h2c` の予約。
- E2E（`tests/api/library_e2e.sh` に `upgrade` 節、io_uring と epoll、平文・TLS（rustls）・kTLS）: 独自プロトコル
  （1 行ずつエコーする `x-echo`）にアップグレードし、ヘッダと同じパケットで送った最初の行が失われないこと・パイプラインの
  2 本目の要求をアップグレード後のバイト列として受けること・同時 1,000 接続・graceful shutdown で閉じること・
  `max_concurrent_connections` に数えられること。HTTP/2・HTTP/3 で 400。
- 非退行: `nonregression` に `veil::api::upgrade` の記号が full バイナリに無いこと。プロキシの WebSocket の E2E（B-144・B-145）が通る。
- 性能: ハンドラの通常の要求（`tools/api-bench` の `/plaintext`・`/json`）が交互 A/B で変わらないこと。

## 文書

- `docs/guide/library.md`（英日）に「Protocol upgrades」の節。Limitations の WebSocket の行を F-262 と合わせて直す。
- C API のヘッダ（`include/veil.h`）のコメント。

## 受け入れ条件

- Rust・C のハンドラで HTTP/1.1 のアップグレードができ、本体のバイナリが変わらない（`nonregression`）。
- 文書（英日）・`backlog.md` が更新されている。
