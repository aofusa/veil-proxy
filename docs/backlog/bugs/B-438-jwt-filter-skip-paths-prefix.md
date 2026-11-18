# B-438: JWT の例の skip_paths が文字列の前方一致で、関係の無い保護されたパスの認証を飛ばしていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「JWT skip_paths uses a raw string prefix match with no segment boundary, so authentication is skipped for unrelated protected paths」から確認。
後の検証の「JWT skip_paths uses a raw string prefix with no segment boundary, so authentication is skipped for unrelated paths like /publish or /healthcare」も同じ指摘。

## 事象

- examples/wasm-filters/jwt-filter/src/lib.rs の `JwtConfig::should_skip` は `:path` を `starts_with` で比べ、セグメントの境界も `.`・`..` も見ていなかった。
- README の例の `skip_paths = ["/health", "/metrics", "/public"]` で `GET /publications/drafts`・`/healthz-admin` 等がトークンなしで上流へ届いた。`[security] path_normalization = "off"` では `/health/../admin` も通った（waf-filter の許可リストは B-410 で直してあった）。

## 対応

- waf-filter の `is_path_whitelisted` と同じ判定にする（クエリを除き、パスそのものか `/` の境界の下だけ。`A-Z a-z 0-9 / - . _ ~` 以外の文字・`.`・`..` のセグメントを含むパスは飛ばさない）。README に書いた。

## テスト

- `tests::skip_paths_respect_segment_boundaries`。
