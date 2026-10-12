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
veil_config_listen(cfg, "0.0.0.0:8080");
veil_register_route(cfg, "/users/{id}", "GET", get_user, NULL);
veil_run(cfg);                                      /* 停止までブロック。cfg は消費される */
```

- ルートのパスは `/users/{id}` のようにパラメータを取れます。`veil_request_param(req, "id", &len)` で取得します。
- OpenAPI は `/openapi.json`・`/docs` に自動で公開されます。`veil_register_route_ex` の `openapi_json` に
  OpenAPI の Operation Object（JSON）を渡すと説明が反映されます。
- 任意の設定は `veil_config_set(cfg, "server.threads", "4")` のように `--override` と同じ構文で上書きできます。
  文字列値は引用符込みで渡します（`"\"warn\""`）。環境変数 `VEIL_OVERRIDE_<n>` も本体と同じ規則で効きます。
- 停止は SIGINT / SIGTERM、または別スレッドからの `veil_shutdown()`。

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

## エラーコード

| 値 | 名前 | 意味 |
|----|------|------|
| 0 | `VEIL_OK` | 成功 |
| -1 | `VEIL_ERR_INVALID_ARG` | NULL・不正な引数 |
| -2 | `VEIL_ERR_CONFIG` | 設定の読み込み・上書きの失敗 |
| -3 | `VEIL_ERR_RUNTIME` | 起動・実行時のエラー（ルート衝突、ポート使用中 など） |
| -4 | `VEIL_ERR_UTF8` | 文字列が UTF-8 ではない |

理由は `veil_last_error()` で取れます。

## テスト

```bash
cargo test -p veil-capi
cargo clippy -p veil-capi --release
```
