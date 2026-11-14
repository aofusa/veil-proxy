# B-399: UDP の L4 のリスナーに並べた Proxy-Wasm のネットワークフィルタが黙って走らなかった

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F35 から確認。

## 事象

- `l4::proxy`（TCP）は `wasm_filter_engine` と `resolved_wasm_modules` でフィルタを走らせるが、`l4::udp` は
  エンジンを参照しない。
- `validate.rs` は UDP の UDS の上流だけを断り、`server.rs` は TLS だけを警告していたので、UDP のリスナーに
  並べたフィルタ（`on_new_connection`・`on_downstream_data` で断るアクセス制御等）は黙って効かなかった。

## 対応

- `validate_config` は `protocol = "udp"` のリスナーに `wasm_modules`・`module_configuration` があれば設定を拒否する
  （「wasm_modules are not supported for UDP (WASM network filters run only on TCP)」）。
- 利用者ガイド（`l4-proxy.md`・`ja`・`security.md`）と `examples/config.toml` に書いた。

## テスト

- `config::validate::b383_tests::udp_l4_rejects_wasm_modules`。
