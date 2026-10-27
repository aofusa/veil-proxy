# B-216: zstd 0.13（`zstd-safe` 7.2.4・`zstd-sys` 2.0.16）に RustSec の勧告 14 件（脆弱性 4・unsound 10）

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の PR で手で回した `container-security-nightly`（`cargo_audit`・`cargo_deny` が failed。2026-10-27）。勧告の公開は 2026-10-16。

## 事象

`Cargo.lock` の `zstd` 0.13.3・`zstd-safe` 7.2.4・`zstd-sys` 2.0.16 に RUSTSEC-2026-0391（使い回したコンテキストで、捨てたストリームの
データが次のストリームへ漏れる）・0398・0403・0410 の脆弱性と、0395〜0397・0399〜0402・0404〜0406 の unsound。

veil は `compression` で `zstd::bulk::Compressor`（スレッドローカルで使い回す）と `zstd::stream::write::Encoder` を使う。
0391 は使い回しの経路に当たる。`zstd` 0.13 は vendoring 版を含む wasmtime の `cache` feature（`wasmtime-internal-cache`）からも入っていた。

## 修正

- `zstd` を 0.14.3（`zstd-safe` 8.2.0・`zstd-sys` 2.1.2）へ上げた。API の使い方は変わらない。
- wasmtime の `cache` feature を外した（veil はコンパイル結果のディスクキャッシュを使わない）。vendoring 版
  （`third_party/wasmtime`）は暗黙のワークスペースメンバーで任意の依存も `Cargo.lock` に載るので、`cache` の feature と
  `wasmtime-internal-cache` の依存をマニフェストから消した（`README.veil.md`）。
- 例（`examples/api/*`）の `Cargo.lock` も更新した。

## 偽 OK

このとき `container-security-nightly` のジョブは成功で終わっていた（`suite_summary.json` で `cargo_audit`・`cargo_deny`・`gitleaks`・
`pumba` が failed）。B-217 で扱う。
