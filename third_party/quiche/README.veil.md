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

### 1. `src/lib.rs`: STREAM フレームの詰め合わせ

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

- `[[example]]`（`examples/` を取り込んでいないため存在しないファイルを指す）を削除した。
- `[lib] crate-type` を `["lib", "staticlib", "cdylib"]` から `["lib"]` にした（F-184）。
  cargo は依存クレートでも列挙された全 crate-type をビルドするため、C 向け FFI の
  静的ライブラリと共有ライブラリを毎回作っていた（veil は Rust API しか使わない）。
  cargo-zigbuild 0.23.4（zig 0.16）の macOS クロスビルドでは、この cdylib のリンクが
  `unable to read exported symbols list '-dead_strip'` で失敗し、`full` の macOS ビルドが
  通らなくなっていた。rlib だけにするとリンク自体が無くなり、全ターゲットでビルドも短くなる。
- あわせて `src/build.rs` の macOS 向けの `cargo:rustc-cdylib-link-arg` を出さないようにした（cdylib が無いので
  意味が無く、macOS のビルドのたびに cargo の警告になる。v0.8.0）。

それ以外は無変更。

### 3b. `src/lib.rs`: 呼び出し側が渡す「今」（`Connection::set_time_hint`、v0.8.0）

`recv`・`send`・`timeout`・`on_timeout` は呼ばれるたびに `Instant::now()` を読む（54KB の応答 1 つで約 60 回）。
veil は 1 回の受信の束・1 接続ぶんの送出の束で 1 回だけ時刻を読み、`set_time_hint(Some(now))` で渡してから
呼び、終わったら `set_time_hint(None)` に戻す。`Connection` に `time_hint` を足し、上の 4 か所の
`Instant::now()` を `self.now()`（ヒントがあればそれ、無ければ `Instant::now()`）にした。

### 3c. `src/h3/mod.rs`: `send_body_zc` が誤った STREAM_DATA_BLOCKED を送る（v0.8.0）

`send_body_zc` は本文のバッファの全長を `stream_send_zc` に渡し、書く量を `Some(body_len)` で絞っていた。
`stream_do_send` は渡されたバッファの全長を「送りたい量」として扱うので、ストリームの容量で `body_len` に
切られると流量制御で止まったと見なし、相手の上限で STREAM_DATA_BLOCKED を送る。その上限は最終的なサイズより
大きいので、ngtcp2（h2load・curl 等）は FINAL_SIZE_ERROR で接続を閉じていた（quinn は無視する）。
`body_len` に切ったバッファを渡し、残りは元のバッファを `split_at(n)` して返すようにした。

### 4. `src/lib.rs`: 予約ビットの検査（B-100）

`Connection::recv_single` で、ヘッダ保護を外した先頭バイトを `decrypt_hdr` 直後に控え、
ペイロードの復号（＝認証）と重複判定の後に予約ビット（ロングヘッダ `0x0c`・ショートヘッダ
`0x18`）が 0 でなければ `Error::InvalidPacket`（= PROTOCOL_VIOLATION の CONNECTION_CLOSE）
を返す。RFC 9000 §17.2 / §17.3.1 の MUST で、upstream は検査していない（h3spec の
「reserved bits in Handshake/Short are non-zero」で検出）。認証後にだけ判定するので、
偽造パケットで接続を切らせることはできない。`grep -n 'veil: RFC 9000' src/lib.rs`。

### 5. aws-lc-sys の Rust のバインディング経由の FFI（`aws-lc-sys` feature、F-243、0.24.9-veil.2）

upstream は BoringSSL の関数を `extern "C" { .. }` で宣言し、その名前のシンボルをリンク時に要求する。veil は Linux で
rustls と同じ aws-lc-sys を使うので、以前は aws-lc-sys を接頭辞無し（`AWS_LC_SYS_NO_PREFIX=1`）でビルドして名前を合わせていた。
この値は環境変数でしか渡せず、veil を依存にする利用者にも `.cargo/config.toml` の `[env]` を要求していた。

- `Cargo.toml`: feature `aws-lc-sys = ["dep:aws-lc-sys"]` と、optional の依存 `aws-lc-sys`（`>=0.45, <0.48`、`ssl`・`bindgen`。
  veil 本体と同じ。`links` があるので必ず 1 つの版になる）。
- `src/veil_ffi.rs`（新規）: マクロ `veil_ffi!`。feature が無ければ今までと同じ `extern "C"` を出す。feature があれば、各宣言を
  同じ名前・同じシグネチャの `#[inline(always)] unsafe fn` にし、`aws_lc_sys::<名前>` を呼ぶ。aws-lc-sys の bindgen は
  名前から接頭辞を外して `#[link_name = "aws_lc_<版>_<名前>"]` を付ける（接頭辞無しのビルドでは付けない）ので、
  接頭辞の有無を aws-lc-sys が吸収する。型の違いは `Conv` で埋める:
  - 不透明な型のポインタ: `cast`
  - コールバック（`Option<extern "C" fn>`）と `*const c_void` で宣言した `SSL_get_ex_new_index` の引数: 大きさを確かめて `transmute_copy`
  - `crypto::Level` ↔ `c_uint`、`ssl_early_data_reason_t` ← `c_uint`（後者は `src/tls/boringssl.rs` の型の隣）
  - `ERR_error_string_n` の戻り値（aws-lc はポインタを返す）は捨てる
  - 名前が違う `sk_num`・`sk_value` は `OPENSSL_sk_num`・`OPENSSL_sk_value` を呼ぶ（`veil_ffi::sys`）
- `src/lib.rs`: `#[macro_use] mod veil_ffi;`。
- `src/tls/mod.rs`・`src/tls/boringssl.rs`・`src/crypto/mod.rs`・`src/crypto/boringssl.rs`・`src/rand.rs`: `extern "C" {` を
  `veil_ffi! {` にしただけ（中の宣言は upstream のまま）。

quiche を上げるときは、この 5 つの宣言の塊を upstream の新しい宣言で置き換えて `veil_ffi! {` に戻す。宣言が増えて
aws-lc-sys に同じ名前が無い・型が合わない場合はコンパイルエラーになるので、`sys` か `Conv` に足す。

## 追従手順

quiche を更新するときは、新バージョンを同じ手順でコピーし直して上記 1〜4 を再適用する
（`grep -n 'Coalesce STREAM frames when fuzzing' src/lib.rs` と `grep -n 'veil:' src/lib.rs`
で該当箇所が見つかる）。
upstream がこの挙動を既定にした場合は vendoring を撤去し `[patch.crates-io]` を消す。

### 3. `src/crypto/boringssl.rs`（警告修正のみ・挙動不変）

`AES_ecb_encrypt` / `CRYPTO_chacha_20` の extern 宣言の戻り値 `-> c_void` を削除した
（C 側は `void` 戻り値。rustc の `c_void_returns` lint が警告する誤った宣言で、
path 依存になると cargo が lint を抑制しなくなるため veil のビルドに警告が出る）。
`[lints.rust]` で allow しない理由: この lint を知らない古い rustc（BSD の pkg 版など）で
`unknown lint` 警告に変わるだけになるため。呼び出し側は戻り値を使っておらず、ABI 上も
`()` と `void` は等価。

## 公開用の変更（F-238）

veil を crates.io に出すため、`[patch.crates-io]` で差し替えるのをやめ、別のパッケージとして公開する。

- `Cargo.toml`: パッケージ名 `veil-proxy-quiche`・版 `0.24.9-veil.2`（`[lib] name = "quiche"` はそのまま。veil.2 は下の 5）。
  既定の feature を空にした（`boringssl-vendored` は取り込んでいない `deps/boringssl` を要るため）。
  `[lints]` で veil の clippy（`-D warnings`）の対象から外す。
- `rustfmt.toml`: `disable_all_formatting = true`（path 依存になり、veil の `cargo fmt` の対象に入ったため）。
- ソースの差分は上の「差分」の節のまま。
