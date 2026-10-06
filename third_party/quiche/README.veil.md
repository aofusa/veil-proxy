# third_party/quiche — veil 向け vendoring（F-172）

## 由来

crates.io の [`quiche` 0.24.9](https://crates.io/crates/quiche/0.24.9) をそのままコピー
（`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/quiche-0.24.9/` から）。
取り込んだのは `Cargo.toml` / `COPYING` / `README.md` / `include/` / `src/` のみ。
`deps/`（boringssl ソース 20MB）・`examples/`・`Cargo.lock`・`Cargo.toml.orig` は
取り込んでいない。veil は quiche を `default-features = false`（Linux）または
`boringssl-boring-crate`（非 Linux）で使い、どちらも `boringssl-vendored` を有効にしない
ため `deps/boringssl` を参照しない。

ルート `Cargo.toml` の `[patch.crates-io]` で全ターゲットの `quiche` をこのディレクトリに
差し替える（パッケージ名・バージョンは crates.io と同一。`Cargo.toml` 側の feature 指定は無変更）。

## 差分

### 1. `src/lib.rs`（挙動の変更はこれだけ）

`Connection::send_single` の STREAM フレーム生成ループ末尾:

```diff
-                #[cfg(feature = "fuzzing")]
-                // Coalesce STREAM frames when fuzzing.
+                // veil: STREAM フレームを常に同一パケットへ詰める（upstream は fuzzing 時のみ）。
                 if left > frame::MAX_STREAM_OVERHEAD {
                     continue;
                 }
```

upstream は fuzzing ビルド以外では **1 パケットに STREAM フレームを 1 つ**しか入れない。
大きなボディでは 1 ストリームでパケットが埋まるので差は出ないが、**小さい応答を多数多重化する
HTTP/3**（h3load `-c64 -m32` で 3B 静的配信）では、1 リクエストにつき 1 パケット
（`sendto` + AEAD 暗号化 + ヘッダ保護）がかかる。

FreeBSD 14.3 aarch64（QEMU/HVF、`tools/perf/freebsd/syscalls_per_req.sh h3_file`）での実測:

| | sendto/req | 合計 syscall/req | 対 nginx スループット |
|---|---|---|---|
| upstream | 1.012 | 1.71 | 0.86 |
| 本差分 | 0.143 | 0.75 | 0.98 |

（nginx は `sendmsg` 0.219/req。複数応答を 1 パケットへ詰めている。）

この経路は upstream の fuzzing ビルドで常用されており（複数フレームを詰めても `left` は
`push_frame_to_pkt!` が減算するため境界は守られる）、ロジック自体は upstream 由来である。
incremental ストリームの末尾への並べ替え（ラウンドロビン）も従来どおり行われる。

### 2. `Cargo.toml`

`[[example]]`（`examples/` を取り込んでいないため存在しないファイルを指す）を削除した。
それ以外は無変更。

## 追従手順

quiche を更新するときは、新バージョンを同じ手順でコピーし直して上記 1・2 を再適用する
（`grep -n 'Coalesce STREAM frames when fuzzing' src/lib.rs` で該当箇所が見つかる）。
upstream がこの挙動を既定にした場合は vendoring を撤去し `[patch.crates-io]` を消す。

### 3. `src/crypto/boringssl.rs`（警告修正のみ・挙動不変）

`AES_ecb_encrypt` / `CRYPTO_chacha_20` の extern 宣言の戻り値 `-> c_void` を削除した
（C 側は `void` 戻り値。rustc の `c_void_returns` lint が警告する誤った宣言で、
path 依存になると cargo が lint を抑制しなくなるため veil のビルドに警告が出る）。
`[lints.rust]` で allow しない理由: この lint を知らない古い rustc（BSD の pkg 版など）で
`unknown lint` 警告に変わるだけになるため。呼び出し側は戻り値を使っておらず、ABI 上も
`()` と `void` は等価。
