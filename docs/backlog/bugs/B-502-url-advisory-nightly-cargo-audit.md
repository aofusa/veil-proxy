# B-502: url 2.5.8 の勧告（RUSTSEC-2026-0592）で main の nightly の cargo-audit・cargo-deny が落ちる

**状態: 修正済み（feat/httparena-perf。2026-11-26 起票・修正）**

## 出典

main（`1f8468dd`）の `container-security-nightly` が 2026-11-22・24・25 の 3 回続けて失敗した（run 45082834492・45322502265・
45529601956。`run.sh (glibc)`・`run.sh (musl)` の `failed phases: cargo_audit cargo_deny`）。192edb00 の 2026-11-21 までは成功していた。

## 事象と修正

| 事象 | 原因 | 修正 |
|---|---|---|
| `url@2.5.8` に RUSTSEC-2026-0592（入れ子の `blob:` の URL で `Url::origin()` がスタックオーバーフロー） | 本体の `Cargo.lock` と `tools/tokio-compat/ecosystem/Cargo.lock` が 2.5.8 | `cargo update -p url --precise 2.5.9`（両方の `Cargo.lock`。`form_urlencoded` も 1.2.3 に上がる） |

- `tools/tokio-compat/ecosystem/Cargo.lock` は、同じ `cargo update` で bindgen 0.72.1 の syn が 2.0.119 に解決し直された
  （bindgen 0.72.1 の要求は `syn = "2.0"`。古い lock は 3.0.9 を指していた）。

## 影響

- P1（nightly が毎回落ちる。公開の `release.yml` の `cargo audit` も全 `Cargo.lock` を見る〈B-158〉ので公開を止める）。
- veil のバイナリには入らない（到達性は無い）。本体の `Cargo.lock` の `url` は vendoring 版 quiche（`third_party/quiche`）の
  dev-dependency（quiche 自身の試験）だけで、`tools/tokio-compat/ecosystem` は tokio 互換の試験用のクレート。

## 関連

- [B-158](B-158-unfixed-image-vulns-and-advisory-gaps.md)（cargo-audit はリポジトリ内の全 `Cargo.lock` を見る）・[B-265](B-265-pulldown-cmark-advisory-vds-server.md)
