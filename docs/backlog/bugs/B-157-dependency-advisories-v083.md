# B-157: v0.8.3 の後に出た依存の RUSTSEC 勧告（rustls-webpki・wasmtime・webpki-roots）と、毎晩の SBOM の失敗

**状態: 完了（fix/v083-regressions、2026-10-22。次の版で公開）**

## 現象

`container-security-nightly` が失敗し続けていた（v0.8.2 の main。v0.8.3 の main でも同じ）。

- `run.sh (glibc)` と `run.sh (musl)`: `cargo_audit` と `cargo_deny` の 2 フェーズが failed（ほかの 25 フェーズは passed）。
  手元で同じ設定（`SKIP_LIBFUZZER=0 SKIP_CARGO_AUDIT=0 SKIP_CARGO_DENY=0 SKIP_GITLEAKS=0 H2SPEC_FULL=1 H3SPEC_REQUIRED=1`）で再現した。
- `SBOM + vulnerability scan`: イメージの SBOM の grype が High で失敗した。中身は distroless（Debian 13）の `libc6`。

## 原因

1. 新しい勧告が出ていた（どれも v0.8.3 の後に出たもの）。

   | クレート | ID | 内容 | 修正版 |
   |---|---|---|---|
   | `rustls-webpki` 0.103.15 | RUSTSEC-2026-0382 | 失効させた中間 CA を受け入れることがある | 0.103.16 |
   | `wasmtime` 36.0.18 | RUSTSEC-2026-0380 | Cranelift のビットの詰め込みの桁あふれで誤コンパイル | 36.0.19 |
   | `wasmtime` 36.0.18 | RUSTSEC-2026-0379 | aarch64 の Cranelift が大きな `br_table` で誤った飛び先へ飛ぶ | 36.0.19 |
   | `webpki-roots` 1.0.8 | RUSTSEC-2026-0381 | 失効させた emSign の中間 CA を信頼する | 1.0.10 |
   | `webpki-root-certs` 1.0.8 | RUSTSEC-2026-0383 | 同上 | 1.0.10 |

   `fxhash`（RUSTSEC-2025-0057、保守終了）は警告だけで、失敗の原因ではない。
2. grype: Debian が「won't fix」とした `libc6` の High は直す版が無く、毎晩失敗し続けるだけだった。ソースの SBOM は High 以上 0 件。

## 修正

- `cargo update`:
  - `rustls-webpki` 0.103.16
  - `webpki-roots`・`webpki-root-certs` 1.0.10
  - `xxhash-rust` 0.8.19（grype の Low、GHSA-6g2r-675j-hx59）
- wasmtime 36.0.19:
  - crates.io の依存の下限を `36.0.19` にした。
  - vendoring 版（`third_party/wasmtime`、`veil-proxy-wasmtime`）を 36.0.19 に取り込み直した。
    - `wasmtime` クレート自体の差は `Cargo.toml` だけで、修正は Cranelift のクレートにある。
    - veil の差分は `patch` で当て直した。`Cargo.toml` のパッケージ名の行だけ手で当てた。
    - `README.veil.md` に経緯を書いた。
- 毎晩のイメージの grype は `only-fixed: true`（直す版がある脆弱性だけで失敗させる。表には全部出る）。

## 確認

- 手元の `run_cargo_audit.sh`: 脆弱性 0 件（`fxhash` の保守終了の警告だけ）。
- 手元の `run_cargo_deny.sh`: `advisories ok, licenses ok`。
- Linux x86_64 の単体 1085 件。
- wasm の E2E。
- FreeBSD aarch64 の VM で、vendoring 版を使うビルドと wasm の単体（`--no-default-features --features full-freebsd`）。
