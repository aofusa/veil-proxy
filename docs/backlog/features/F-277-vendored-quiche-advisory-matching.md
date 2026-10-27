# F-277: vendoring 版 quiche（`veil-proxy-quiche`）の RustSec 勧告を `cargo audit` で照合できるようにする

**状態: 未着手（2026-10-27 起票。セキュリティレビュー SC-04）**

## 今の状態

- `third_party/quiche` は `[package] name = "veil-proxy-quiche"`（F-238 の crates.io の名前の衝突を避けた）で、`Cargo.lock` にもその名前で載る。
- RustSec の advisory は crates.io の名前（`quiche`）に付くので、`cargo audit`・`cargo deny` は vendoring 版に**一致しない**。quiche の脆弱性は
  人手で追うしかなく、B-158 の「全 Cargo.lock を失敗で検査」の網から外れている。
- `third_party/wasmtime`（`veil-proxy-wasmtime`）は crates.io 版の `wasmtime` も lock に残るので一致する。`third_party/quinn-udp` は試験用。

## 改修内容

- `tools/container_security/security/run_cargo_audit.sh` に、`third_party/*/Cargo.toml` の元の名前と版（`README.veil.md` に書く）を RustSec の
  DB（`~/.cargo/advisory-db` の `crates/quiche/*.md`）と突き合わせる段を足し、該当があれば失敗にする。`release.yml` の勧告の関門も同じ。
- または vendoring 版の `[package] name` を `quiche` に戻し、`[patch.crates-io]` で差し替える（crates.io への公開は `veil` 本体だけなので、
  F-238 の衝突とは別。`cargo publish` が path 依存を拒むかを確かめる）。

## 受け入れ条件

- 試験用に古い版の advisory（例: quiche の既知の勧告）を一時的に DB へ置き、`run_cargo_audit.sh` が失敗する。

## 関連

- [B-158](../bugs/B-158-unfixed-image-vulns-and-advisory-gaps.md)、[F-172](F-172-quiche-stream-frame-coalescing.md)（vendoring の経緯）、
  B-216（zstd の勧告。feat/v0100 で修正済み）
