# veil-capi-helpers — C の helpers kit `libveil_kit`（F-291）

Rust の [`veil-proxy-kit`](../veil-kit/)・Python の `veil-api-kit` と同じ補助（Cookie・署名付き/暗号化 Cookie・セッション・
Basic / API キー / JWT の認証・主体と scope の検査・リダイレクト・ファイル・定期タスクと cron）を C から使うためのライブラリです。
ヘッダは [include/veil_kit.h](include/veil_kit.h)（`veil.h` を include する）。関数の接頭辞は `veil_kit_`。

`libveil_kit` は [`libveil`](../veil-capi/README.md) の C API を全部含む上位集合です。プログラムは **どちらか 1 つ** とリンクします
（両方をリンクすると同じ記号が 2 つになる）。プロキシ本体・veil の既定の feature・`full*` には入りません。

## ビルドとリンク

```bash
cargo build -p veil-capi-helpers --release
# 成果物: target/release/libveil_kit.so（動的）と target/release/libveil_kit.a（静的）
gcc app.c -Icrates/veil-capi/include -Icrates/veil-capi-helpers/include -Ltarget/release -lveil_kit \
    -lpthread -ldl -lm -Wl,-rpath,$PWD/target/release -o app
```

動くサンプルは [examples/api/c-kit/](../../examples/api/c-kit/)（`make`）。`tests/api/library_e2e.sh c-kit` が動かし、
Cookie の署名と HS256 のトークンを Python の標準ライブラリだけの独立した実装とも突き合わせます。

## Rust の kit との互換

- Cookie の鍵 `veil_kit_key_derive(secret)` は Rust の `Key::derive_from(secret)`、`veil_kit_key_from(64 バイト)` は `Key::from`
  と同じ鍵になります。署名付き Cookie（`base64url(HMAC-SHA256)` + `.` + 値）と暗号化 Cookie（`base64url(nonce || AES-256-GCM)`、
  名前が関連データ）は 1 バイトも違わない形式なので、Rust と C のサービスが互いの Cookie を読めます。
- セッションは Rust の kit の `Sessions` をそのまま入れるので、Cookie の形式・メモリ保存の扱いも同じです。
- JWT は Rust の kit の `JwtVerifier` / `encode_hmac` を使います（Rust の kit の `Algorithm` の全部: HS・RS・PS・ES 系、JWKS）。

## 規則

| 対象 | 規則 |
|------|------|
| `veil_kit_free` で解放しない文字列（`veil_kit_cookie_get`・`veil_kit_session_get`・`veil_kit_form_get` など） | NUL 終端ではない（長さを使う）。コールバックが戻るまで有効。 |
| 「`veil_kit_free` で解放する」文字列 | NUL 終端。呼び出し側のもの。 |
| `veil_kit_key_t *` / `veil_kit_jwt_verifier_t *` / `veil_kit_principal_t *` | `_free` で解放する。設定に渡したもの（セッション・JWT のガード）は設定が自分の参照を持つので、`veil_run` の後に解放してよい。 |
| ガード（`veil_kit_jwt_authenticate`・`veil_kit_require_scope`） | C のミドルウェアとして動く。後に登録したものが先に動くので、同じ接頭辞では `require_scope` を先に登録する。 |
| `veil_kit_send_file` | ハンドラ用プールのルートだけ（`VEIL_ROUTE_INLINE` では `VEIL_ERR_STATE`）。 |
| タスク（`veil_kit_every`・`veil_kit_cron`） | 予定は 1 個のワーカーが数え、C の関数はハンドラ用プールで呼ぶ。前の回が終わってから次を数え、停止が始まったら次を始めない。 |

エラーコードは `libveil` と同じ（理由は `veil_last_error()`）。加えて `VEIL_KIT_ERR_TOKEN`（-20）はトークンが不正なとき。

## まだ無いもの

HTTP・gRPC のクライアント、遅延応答、JSON の本文の検査（422）、プロセス内の試験呼び出し、依存性注入、gRPC の `Balanced`、
GraphQL、配布物（zip・pkg-config・CMake）は後の版で扱います。

## テスト

```bash
cargo test -p veil-capi-helpers
cargo clippy -p veil-capi-helpers --all-targets
tests/api/library_e2e.sh c-kit
```
