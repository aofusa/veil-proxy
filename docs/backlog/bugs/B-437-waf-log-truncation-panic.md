# B-437: WAF の例が検出の記録で値をバイト 50 で切り、多バイト文字の途中で panic してフィルタが落ちた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Slicing the attacker-derived matched value at byte 50 panics on a multibyte char and traps the WAF」から確認。

## 事象

- examples/wasm-filters/waf-filter/src/lib.rs の `on_http_request_headers` は検出の記録（`log::warn!`）で `matched_value` を `[..min(50, len)]` で切っていた。`url_decode` は `%80`〜`%FF` を 2 バイトの文字にするので、デコードした値の文字の途中で切れて panic した。
- `GET /?a%C3%C3...%3Cscript%3E` で Wasm のインスタンスが落ち、`failure_policy = "fail_open"` なら要求がそのまま上流へ届き、既定の `fail_closed` でも 503 とインスタンスの作り直しになった。

## 対応

- 文字の境界で切る（`truncate_on_char_boundary`）。

## テスト

- `tests::truncation_keeps_char_boundaries`。
