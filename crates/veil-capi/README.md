# veil-capi — veil API サーバ基盤ライブラリの C API（F-193）

C（および C ABI を話せる言語）から veil の HTTP/1.1・HTTP/2・HTTP/3 サーバを使うための共有/静的ライブラリです。
C のコールバックを HTTP ハンドラとして登録し、`veil_run()` で起動します。ヘッダは [include/veil.h](include/veil.h)。

## ビルド

```bash
cargo build -p veil-capi --release
# 成果物: target/release/libveil.so（動的）と target/release/libveil.a（静的）
```

配布向けは `--profile dist`（fat LTO）を使います。ホストプロセスのアロケータは置き換えません
（`veil-alloc` / `mimalloc` を含めない）。

## リンク

```bash
gcc hello.c -Icrates/veil-capi/include -Ltarget/release -lveil -lpthread -ldl -lm \
    -Wl,-rpath,$PWD/target/release -o hello
```

静的リンクは `libveil.a` を直接指定します（`-lpthread -ldl -lm` は必要）。
動くサンプルは [examples/api/c-hello/](../../examples/api/c-hello/)（`make` で作れます）。

## 使い方の流れ

```c
veil_config_t *cfg = veil_config_new();            /* または veil_config_from_file("config.toml") */
veil_config_listen_cleartext(cfg, "0.0.0.0:8080");   /* 平文 HTTP/1.1 + h2c を外部に公開（既定は 127.0.0.1:8080）。TLS は veil_config_listen + veil_config_tls */
veil_register_route(cfg, "/users/{id}", "GET", get_user, NULL);
veil_run(cfg);                                      /* 停止までブロック。cfg は消費される */
```

- ルートのパスは `/users/{id}` のようにパラメータを取れます。`veil_request_param(req, "id", &len)` で取得します。
- OpenAPI は `/openapi.json`・`/docs` に自動で公開されます。`veil_register_route_ex` の `openapi_json` に
  OpenAPI の Operation Object（JSON）を渡すと説明が反映されます。
- 任意の設定は `veil_config_set(cfg, "server.threads", "4")` のように `--override` と同じ構文で上書きできます。
  文字列値は引用符込みで渡します（`"\"warn\""`）。環境変数 `VEIL_OVERRIDE_<n>` も本体と同じ規則で効きます。
- アクセスログは既定で標準出力に出ます（`[access_log] enabled = true`・`format = "text"`）。止めるときは
  `veil_config_set(cfg, "access_log.enabled", "false")`。
- 停止は SIGINT / SIGTERM、または別スレッドからの `veil_shutdown()`。
- プロトコルの違うポートを複数開くときは `veil_listener_t` を埋めてポートごとに `veil_config_add_listener` を呼びます
  （`[[listener]]` の 1 項目）。`http2`・`allow_http`・`allow_h2c`・`secure_redirect` は負の値で既定、文字列は `NULL` で省略です。
  1 個でも足すと、既定の `127.0.0.1:8080` は `veil_config_listen_cleartext` を呼ばない限り開きません。

  ```c
  veil_listener_t l = { .listen = "0.0.0.0:8443", .tls = 1, .http2 = -1, .http3 = 1,
                        .allow_http = -1, .allow_h2c = -1, .secure_redirect = -1 };
  veil_config_add_listener(cfg, &l);
  ```

## スレッドモデル

- **既定**: コールバックは **ハンドラ用プール**（`[api] handler_threads`）のスレッドで実行されます。
  ワーカースレッド（ネットワーク I/O を担当）をブロックしないため、DB 呼び出しなど時間のかかる処理も書けます。
  ただしコールバックは **複数スレッドから同時に呼ばれます**。
- **`VEIL_ROUTE_INLINE`**: ワーカースレッド上で直接実行します（スレッド切り替えのコストが無い）。
  ブロックや長い CPU 処理をするとそのワーカーの全接続が止まります。短時間で終わる処理だけに使ってください。
- ハンドラが非 0 を返す、またはコールバック内で panic 相当の異常があると、500 を返します。

## 所有権とライフタイム

| 対象 | 規則 |
|------|------|
| `veil_config_t *` | `veil_config_new` / `veil_config_from_file` で得て、`veil_run` に渡す（**消費される**）か `veil_config_free` で解放する。両方はしない。 |
| `veil_request_t` / `veil_response_t` | コールバックの引数としてのみ有効。保持・別スレッドへ渡さない。 |
| リクエストアクセサの戻り値 | **NUL 終端ではない**。長さは `size_t *len`。コールバックから戻るまで有効。見つからなければ `NULL` と長さ 0。コピーが必要なら自分で行う。 |
| `veil_response_set_*` の引数 | ライブラリがコピーする（呼び出し後に解放してよい）。ヘッダ名・値の CR/LF 等は拒否（`VEIL_ERR_INVALID_ARG`）。 |
| `user_data` | ライブラリは解釈せずそのまま返す。**サーバ稼働中ずっと有効で、複数スレッドから同時に使っても安全**であること（呼び出し側が保証する）。 |
| `veil_last_error()` | スレッドローカル。次のエラーまで有効。解放しない。 |

## ストリーミング・gRPC・L4（F-210）

veil はバイト列を運ぶだけで、メッセージの直列化（protobuf-c・nanopb・upb 等）は利用者が行います。
動くサンプルは [examples/api/c-features/](../../examples/api/c-features/)（`make` で作れます）。

### 応答と要求本文のストリーミング

```c
static int events(const veil_request_t *req, veil_response_t *resp, void *ud) {
    veil_response_set_header(resp, "content-type", "text/event-stream");
    veil_stream_t *s = veil_response_stream(resp);      /* ステータス・ヘッダを送る */
    for (int i = 0; i < 3; i++)
        if (veil_stream_write(s, (const uint8_t *)"data: tick\n\n", 12) == VEIL_ERR_CLOSED) break;
    veil_stream_close(s);                                /* 必須。別スレッドに渡して後で閉じてもよい */
    return 0;
}
/* 要求本文を届いた順に読む: veil_register_route_ex(..., VEIL_ROUTE_STREAM_BODY) と */
int64_t n; uint8_t buf[16384];
while ((n = veil_request_body_read(req, buf, sizeof buf)) > 0) { /* ... */ }
```

### gRPC

```c
static int say_hello(veil_grpc_call_t *call, void *ud) {
    const uint8_t *msg; size_t len;
    if (veil_grpc_recv(call, &msg, &len) != 1) {
        veil_grpc_set_status(call, 3, "missing request");     /* INVALID_ARGUMENT */
        return 0;
    }
    /* msg を復号して応答を符号化 */
    veil_grpc_send(call, out, out_len);
    return 0;                                            /* veil_grpc_set_status の状態（未設定なら OK）で終える */
}
veil_register_grpc(cfg, "/demo.v1.Demo/SayHello", VEIL_GRPC_UNARY, say_hello, NULL);
veil_register_grpc_health(cfg);                          /* grpc.health.v1.Health */
```

- `veil_grpc_recv`: 1 = 受け取った（ポインタは次の recv かコールバックの終わりまで有効）、0 = 終わり、負 = エラー。
- 単項・クライアントストリーミングの `veil_grpc_send` は 1 回だけ（2 回目は `VEIL_ERR_STATE`）。
- メタデータ・期限: `veil_grpc_metadata`・`veil_grpc_method`・`veil_grpc_deadline_ms`。応答ヘッダ・トレーラ:
  `veil_grpc_set_header`・`veil_grpc_set_trailer`。
- 圧縮・期限・gRPC-Web・`[api] grpc_*` の設定は Rust のハンドラと同じで、HTTP/2・HTTP/3 の両方で動きます。

### L4（TCP・UDP）

```c
static int on_data(veil_l4_conn_t *c, const uint8_t *d, size_t n, void *ud) { return veil_l4_write(c, d, n); }
static const veil_l4_tcp_callbacks_t cb = { .on_data = on_data };
veil_register_l4_tcp(cfg, "echo", "127.0.0.1:7000", &cb, NULL);
```

コールバックはハンドラ用プールで、接続ごとに 1 つずつ順に呼ばれます（`veil_l4_write`・`veil_l4_close` はコールバックの中でだけ）。
接続ごとのデータは `veil_l4_set_conn_data` / `veil_l4_conn_data`。UDP は `veil_register_l4_udp` のコールバックで
`veil_l4_reply` を呼ぶと応答します。

### HTTP/1.1 のプロトコルアップグレード（F-261）

```c
static int on_data(veil_l4_conn_t *c, const uint8_t *d, size_t n, void *ud) { return veil_l4_write(c, d, n); }
static const veil_l4_tcp_callbacks_t cb = { .on_data = on_data };

static int upgrade(const veil_request_t *req, veil_response_t *resp, void *ud) {
    return veil_response_upgrade(resp, "x-echo", &cb, NULL);   /* 101 → 以後は L4 と同じコールバック */
}
```

`veil_response_upgrade` は応答を `101 Switching Protocols`（`Upgrade: x-echo`）にし、101 を書いた後の接続を L4 の TCP と同じ
コールバック（`on_connect` → `on_data` … → `on_close`、`veil_l4_write`・`veil_l4_close`）で受けます。ヘッダの直後に届いていた
バイト列も最初の `on_data` に渡ります。要求に `Connection: upgrade` と `Upgrade` が無い・本文付き・HTTP/1.0・HTTP/2・HTTP/3 なら
veil が 400 に置き換えます（`h2c` は予約済みで 500）。

### 所有権

| 対象 | 規則 |
|------|------|
| `veil_stream_t *` | `veil_stream_close` で必ず閉じる（閉じるまで応答は終わらない）。コールバックの後も別スレッドで使える。 |
| `veil_grpc_call_t *` / `veil_l4_conn_t *` / `veil_l4_reply_t *` | コールバックの中でだけ有効。 |
| `VEIL_ROUTE_INLINE` のルート | ストリーミング・本文の読み取りは使えない（`VEIL_ERR_STATE` / `NULL`）。 |

## エラーコード

| 値 | 名前 | 意味 |
|----|------|------|
| 0 | `VEIL_OK` | 成功 |
| -1 | `VEIL_ERR_INVALID_ARG` | NULL・不正な引数 |
| -2 | `VEIL_ERR_CONFIG` | 設定の読み込み・上書きの失敗 |
| -3 | `VEIL_ERR_RUNTIME` | 起動・実行時のエラー（ルート衝突、ポート使用中 など） |
| -4 | `VEIL_ERR_UTF8` | 文字列が UTF-8 ではない |
| -5 | `VEIL_ERR_CLOSED` | 相手が切断した・ストリームが閉じている（F-210） |
| -6 | `VEIL_ERR_STATE` | 呼べない状態（INLINE でのストリーミング・単項の 2 回目の送信等） |

理由は `veil_last_error()` で取れます。

## テスト

```bash
cargo test -p veil-capi
cargo clippy -p veil-capi --release
```
