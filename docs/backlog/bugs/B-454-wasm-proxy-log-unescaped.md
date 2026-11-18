# B-454: Proxy-Wasm の `proxy_log` がゲストの文字列（クライアントの値を含むことが多い）を制御文字のままアプリのログへ書いていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Proxy-Wasm proxy_log writes guest-supplied text, which often contains client data, unescaped into the default text
application log」から確認。

## 事象

- `wasm::host::logging` の `proxy_log` はゲストのメモリの UTF-8 の文字列をそのまま ftlog へ渡し、既定の `text` の形式は
  そのまま書いていた。プラグインがクライアントの値を書くと（同梱の WAF の例は URL を復号した一致の値を書く）、改行で
  偽のログの行（veil のエラー・監査の記録に見えるもの）を足したり、端末のエスケープシーケンスを埋めたりできた。veil の
  テキストのアクセスログは `write_text_escaped` で制御文字を書き換えている。
- WASI の `fd_write`（stdout・stderr）と、gRPC・HTTP の呼び出しのログに書くゲストの上流名・サービス名・メソッド名も
  同じだった。

## 対応

- ゲストの文字列は `LogSafe` を通して書く（`\n`・`\r`・`\t` と、他の制御文字（C0・DEL・C1）を `\u{..}` で書く。普通の
  文字と非 ASCII はそのまま）。`proxy_log`・`fd_write`・`allowed_upstreams` で断った上流名・gRPC のサービス名・メソッド名に
  使う。
- 利用者ガイド（`wasm.md` の英日の `allow_logging`）に書いた。

## テスト

- `wasm::host::logging::tests::log_safe_escapes_control_characters`。
