# B-342: `[prometheus] enabled` の型の文書が既定値を `true` と書いている

**状態: 修正済み（feat/v0120-perf、2026-11-08）**

v0.12.0 の作業で設定の型を読んでいて見つけた。

## 事象

`src/config/prometheus.rs` の `PrometheusConfig::enabled` の文書コメントが「デフォルト: true」と書いているが、既定値
（`default_prometheus_enabled`）は `false`。利用者向けの `docs/guide/configuration.md`（英日）と `examples/config.toml` は
`false` で正しい。挙動は変わらない（文書だけの食い違い）。

## 原因

既定値を `false` に変えたときに、型のフィールドの文書コメントだけが残った（`src/config.rs` を分割した F-233 より前から）。

## 対応

文書コメントを「デフォルト: false」に直す。コードの挙動・設定キーは変えない。
