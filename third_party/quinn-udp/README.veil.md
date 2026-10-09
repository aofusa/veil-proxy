# third_party/quinn-udp — veil 向け vendoring（F-176 / B-62）

## 由来

crates.io の [`quinn-udp` 0.5.14](https://crates.io/crates/quinn-udp/0.5.14) をコピー
（`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/quinn-udp-0.5.14/` から）。
取り込んだのは `Cargo.toml` / `build.rs` / `src/` / ライセンス。`benches/`・`tests/`・
`Cargo.lock`・`Cargo.toml.orig` は取り込まず、`Cargo.toml` から `[[test]]`・`[[bench]]`・
`[dev-dependencies]` を削除した。

ルート `Cargo.toml` の `[patch.crates-io]` で差し替える。**veil 本体は quinn-udp を使わない**
（E2E・単体テスト用の HTTP/3 クライアント `quinn` / `h3-quinn` が依存する dev 専用の依存）。

## 差分

### `src/lib.rs` / `src/unix.rs` / `src/cmsg/unix.rs`: NetBSD/aarch64 の CMSG アラインメント

libc クレート（0.2.189 時点でも未修正）は NetBSD/aarch64 の `_ALIGNBYTES` を
`size_of::<c_int>() - 1`（3）としているが、カーネルの値は `long` 由来の 7。`libc::CMSG_*` で
組み立てた cmsg がカーネルと 4 バイトずれ、quinn-udp が受信直後に panic して
`quinn::Endpoint` の Drop 内で二重 panic し、テストバイナリごと落ちていた（B-62）。

`crate::cmsg_sys` を追加し、NetBSD/aarch64 だけ正しいアラインメントで `CMSG_DATA` /
`CMSG_LEN` / `CMSG_SPACE` / `CMSG_NXTHDR` を実装する（他の target は `libc` の再エクスポート）。
`libc::CMSG_*` の呼び出しをすべて `crate::cmsg_sys::CMSG_*` へ置き換えた。

libc の upstream が修正されたら、この vendoring は削除してよい。

### `build.rs`: cfg_aliases を使わない

path 依存では依存クレートの lint が cap されず、`cfg_aliases!` マクロ内部の警告が出続けるため、
同じ cfg 別名（`apple` / `bsd` / `solarish` / `apple_fast` / `apple_slow` / `wasm_browser`）を
`build.rs` で直接定義し、`[build-dependencies] cfg_aliases` を外した。
