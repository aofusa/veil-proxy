# B-36: VEIL_TLS_INSECURE が per-upstream tls_insecure=false を上書きする

## 事象

`[upstreams."strict-cert-pool"]` で `tls_insecure = false` を指定しても、環境変数 `VEIL_TLS_INSECURE=1`（E2E 起動時に設定）があると自己署名バックエンドへ接続成功し **200** が返る。

## 再現

1. `VEIL_TLS_INSECURE=1 veil --config proxy.toml`（strict-cert-pool あり）
2. `curl -ks https://proxy/strict-cert/` → 200（期待: 502）

`VEIL_TLS_INSECURE` 未設定時は 502（期待どおり）。

## 影響

- 厳密証明書検証の per-upstream 設定が E2E/開発環境で無効化される
- セキュリティ意図と実挙動の乖離

## 改修案

`VEIL_TLS_INSECURE` は **クライアント向け自己署名許可** に限定し、上流 `tls_insecure = false` より優先しない。`proxy.rs` の `upstream_group.tls_insecure() || env` 結合を見直す。

## 対応状況

完了（2026-07-08）: `proxy.rs` / `http3_server.rs` / `http3_stream.rs` から上流経路の `VEIL_TLS_INSECURE` OR を削除。HTTPS プールキーに `tls_insecure` タグを追加しプール汚染を防止。E2E の `/echo-upload-tls/*` を `tls-echo-pool`（`tls_insecure=true`）へ移行。`test_e2e_upstream_strict_cert_rejects` の `#[ignore]` を解除。