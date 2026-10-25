# F-262: API ライブラリのハンドラで WebSocket を終端する（Rust・C）

**状態: 未着手（2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)。前提は [F-261](F-261-handler-http11-upgrade.md)。Python は [F-263](F-263-python-async-runtime-websocket.md)）**

## 背景

- veil の WebSocket は「プロキシとしての中継」だけ（`src/proxy/websocket.rs`。101 を作るのもフレームを読むのも上流）。
  フレームの符号化・`Sec-WebSocket-Accept` の計算はコードのどこにも無い。
- `docs/guide/library.md` の Limitations・`crates/veil-python/docs/websocket.md` は「ハンドラは WebSocket を終端できない」と書いている。
- HttpArena（[F-269](F-269-benchmark-submissions.md)）の echo-ws（512/4096/16384 接続）・echo-ws-pipeline（16 件まとめ送り）・
  echo-ws-limited（10 メッセージごとに切断＝ハンドシェイクの費用）は、フレームワーク標準の WebSocket API で `/ws` のエコーを求める。

## 制約

- ライブラリ利用時だけ（F-261 と同じ。本体のバイナリは変わらない。プロキシの `websocket` feature の中継には触れない）。
- ホットパス規則: メッセージごとの確保・複製を増やさない。受信はプールのバッファで受けてその場でマスクを外し、`Bytes` の切り出しで渡す。
  送信はフレームのヘッダ（2〜10 バイト）と本文を `writev` 1 回で送る（本文を連結し直さない）。

## 設計

### 1. API（`veil::api::ws`、feature `handler`。依存は増やさない）

```rust
#[veil::route(path = "/ws", method = "GET")]
async fn ws(upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .protocols(["chat.v1"])                 // 任意: Sec-WebSocket-Protocol の選択
        .max_message_size(1 << 20)              // 任意: 既定は [api] websocket_max_message_size
        .on_upgrade(|mut ws: WebSocket| async move {
            while let Some(Ok(msg)) = ws.recv().await {
                match msg {
                    Message::Text(t) => { let _ = ws.send(Message::Text(t)).await; }   // t は Utf8Bytes（Bytes の薄い包み）
                    Message::Binary(b) => { let _ = ws.send(Message::Binary(b)).await; }
                    Message::Close(_) => break,
                    _ => {}                                                            // Ping は自動で Pong
                }
            }
        })
}
```

- `WebSocketUpgrade`: 抽出子（`FromRequest`）。HTTP/1.1・`GET`・`Upgrade: websocket`・`Connection: upgrade`・
  `Sec-WebSocket-Version: 13`・16 バイトの `Sec-WebSocket-Key`（base64）を検査し、違えば 400（版違いは 426 + `Sec-WebSocket-Version: 13`）。
  `origin(&[..])` で許す Origin を絞れる（既定は検査しない。CSRF の注意を文書に書く）。
- `on_upgrade` は F-261 の `Response::switching_protocols("websocket", ..)` に `Sec-WebSocket-Accept`
  （SHA-1(key + GUID) の base64。SHA-1 は TLS のプロバイダの crate（Linux 等は aws-lc-rs、OpenBSD/NetBSD は ring）の
  `SHA1_FOR_LEGACY_USE_ONLY` を使い依存を増やさない）と選んだ `Sec-WebSocket-Protocol` を付ける。
- `WebSocket`: `recv()`・`send(Message)`・`send_batch(&[Message])`（まとめて 1 回の `writev`）・`close(code, reason)`・
  `split()`（読み手と書き手。書き手は `Clone` できる送信キュー。別タスクから送る用途）・`ping(Bytes)`。
  `Message = Text(Utf8Bytes) | Binary(Bytes) | Ping(Bytes) | Pong(Bytes) | Close(Option<CloseFrame>)`。
- マクロ・OpenAPI: `WebSocketUpgrade` を引数に持つルートは OpenAPI に `GET` + 応答 `101` で載せる（`x-veil-websocket: true`）。

### 2. フレームの処理（`src/api/ws/codec.rs`）

- RFC 6455 の全部: FIN と断片化（継続フレームの組み立て。組み立て中も制御フレームを割り込ませてよい）、クライアントからの
  マスク必須（無ければ 1002 で閉じる）、予約ビット（拡張無しなら 0 以外で 1002）、制御フレームは 125 バイト以下・断片化しない、
  テキストの UTF-8 検査（断片をまたぐ途中の状態を持つ。不正なら 1007）、Close の往復（受けたら同じ code で返して閉じる）、
  上限超えは 1009、未知の opcode は 1002。
- **受信**: 接続ごとに 1 個の読み取りバッファ（`src/pool.rs` のプールから借りる。キープアライブ待ちと同じく、待っている間は
  バッファを持たない作りを検討する（16,384 接続 × 16KB を常駐させない。B-101 の教訓）。readiness を待ってから借りる）。
  1 フレームがバッファに収まる限り、マスクをその場で外して `Bytes` で切り出す（複製しない）。断片化したメッセージと大きな
  フレームだけ組み立て用の `BytesMut` を確保する。マスク外しは 8 バイト単位（`u64` の XOR）にする。
- **送信**: サーバからのフレームはマスクしないので、ヘッダだけ作り本文の `Bytes` と並べて `writev`。`send` が並んだとき
  （分割した書き手・`send_batch`）は 1 回の `writev` にまとめる（IOV_MAX まで）。echo-ws-pipeline のように 16 件まとめて届いたら、
  ハンドラが続けて `send` した分を次の `recv` の前に 1 回で送る（書き込みのコアレッシング。今の HTTP/2 の F-74 と同じ考え）。
- **Ping/Pong**: Ping を受けたら自動で Pong（`auto_pong(false)` で止められる）。`[api] websocket_ping_interval_secs`（既定 0 = 送らない）で
  サーバから Ping を送り、`websocket_idle_timeout_secs` で応答の無い接続を閉じる。
- **permessage-deflate（RFC 7692）**: 第 2 段（同じチケットの後半）。`[api] websocket_compression = false`（既定 off）で、
  `compression` feature の flate2 を使う。文脈の引き継ぎ（`no_context_takeover`）の交渉・圧縮の上限（zip bomb）。
  第 1 段は拡張を交渉しない（`Sec-WebSocket-Extensions` を無視）。
- graceful shutdown: `shutdown_signal()` で 1001（Going Away）の Close を送ってから閉じる。

### 3. tokio の生態系

- F-261 の `Upgraded` が `veil::tokio::io` の trait を実装するので、tokio-tungstenite・fastwebsockets も `veil::tokio` 上で動く
  （`tools/tokio-compat/ecosystem` に 1 件足して確かめる）。axum の `WebSocketUpgrade` は hyper の `OnUpgrade` を要るので、
  `tokio_service` の橋渡しで `OnUpgrade` を渡す対応は範囲外（文書に書き、ネイティブの API か tokio-tungstenite を案内）。

### 4. C API

- `veil_ws_accept(req, resp, const veil_ws_callbacks_t *cb, void *ud)`: `on_message(ws, opcode, ptr, len)`・`on_close(ws, code)`、
  `veil_ws_send(ws, opcode, ptr, len)`（複製して送る。どのスレッドからでも）・`veil_ws_close(ws, code)`。コールバックはプールで実行
  （L4 と同じイベント型。プールを長く占有しない）。

### 5. 設定（`[api]`、ライブラリ専用）

| キー | 既定 | 意味 |
|---|---|---|
| `websocket_max_message_size` | 16 MiB | 組み立て後のメッセージの上限（超えたら 1009） |
| `websocket_max_frame_size` | 16 MiB | 1 フレームの上限 |
| `websocket_ping_interval_secs` | 0 | サーバからの Ping の間隔（0 = 送らない） |
| `websocket_idle_timeout_secs` | 0 | 受信の無い接続を閉じるまで（0 = 無し） |
| `websocket_compression` | false | permessage-deflate（第 2 段） |

## 試験

- 単体: フレームの符号化・復号（断片化・制御フレームの割り込み・マスク・UTF-8 の断片またぎ・上限・予約ビット・未知の opcode）、
  `Sec-WebSocket-Accept` の RFC 6455 §1.3 の例、ハンドシェイクの検査（版違い 426・鍵の長さ）。ファジング: `fuzz/` に復号器の対象を 1 個。
- **Autobahn TestSuite**（`crossbario/autobahn-testsuite` の fuzzingclient、1.* 〜 7.*・9.*（性能）・12/13（圧縮、第 2 段））を
  `tests/api/library_e2e.sh websocket` から回し、全項目 OK / NON-STRICT（INFORMATIONAL を除く）。io_uring・epoll・TLS。
- E2E: 16,384 接続のエコー（`ulimit` を上げて）、16 件まとめ送りの往復、10 メッセージごとの切断の繰り返し（ハンドシェイクの費用）、
  graceful shutdown で 1001。`tools/perf/leak_check.sh` の接続チャーンに WebSocket の開閉を足し、生存ヒープ・fd が増えないこと。
- 性能: `tools/api-bench` に echo（HttpArena の echo-ws と同じ形。負荷は `tools/api-bench` に小さな WebSocket の負荷生成器を足すか
  HttpArena の gcannon を使う）を足し、tokio + axum（`axum::extract::ws`）・tokio + fastwebsockets と交互に比べる。目標は axum 以上。
- 非退行: `nonregression`（`veil::api::ws` の記号が full に無い）、プロキシの WebSocket 中継の E2E・性能が変わらない。

## 文書

- `docs/guide/library.md`（英日）に「WebSocket」の節（API・設定・Origin の注意・HTTP/2 では使えないこと・tokio-tungstenite の使い方）、
  Limitations の更新。C API の節。`examples/api/rust-websocket`（エコーとブロードキャストのチャット。`split` と送信キュー）。

## 範囲外（後続の候補）

- RFC 8441（HTTP/2 の拡張 CONNECT）・RFC 9220（HTTP/3）での WebSocket。axum の `WebSocketUpgrade` の橋渡し。

## 受け入れ条件

- Autobahn の fuzzingclient が通り、HttpArena の echo-ws 系の validate を満たす。
- 本体のバイナリが変わらない（`nonregression`）。文書（英日）・`backlog.md` が更新されている。
