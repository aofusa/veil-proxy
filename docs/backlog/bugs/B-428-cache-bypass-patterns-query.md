# B-428: キャッシュの `bypass_patterns` が要求の対象全体（クエリ込み）に当たり、`?` を足すだけで除外を外れて同じキーに保存された

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「A trailing '?' gets around cache bypass_patterns, so excluded per-user responses are stored and served from the
shared cache」から確認。

## 事象

- `CacheConfig::should_bypass` は glob を要求の対象全体（`proxy::forward` の `path_str`。クエリ込み）に当てていた。
  一方でキャッシュのキーはクエリの前のパスで作り、空のクエリは無いのと同じキーになる。
- `bypass_patterns = ["/cached-api/session"]`（例の設定）でも `/cached-api/session?` は除外に一致せず、利用者ごとの
  応答（`Cache-Control` の無いもの）を `/cached-api/session` と同じキーで保存し、別の利用者へ返し得た。

## 対応

- パターンはクエリの前のパスだけに当てる。
- 利用者ガイド（`compression-cache.md` の英日）と `examples/config.toml` に書いた。

## テスト

- `cache::config::tests::test_bypass_patterns` に空のクエリ・付け足したクエリの例を足した。
