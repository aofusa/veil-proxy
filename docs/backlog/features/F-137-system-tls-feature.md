# F-137: `system-tls` feature（rustls / quiche のシステム SSL 動的リンク）

## 取り下げ（Withdrawn、2026-07-30）

**本チケットの実装はユーザー判断により撤回済み。** `system-tls`/`vendored-tls`/
`openbsd-vendor-tls`/`netbsd-vendor-tls` などの feature、`build.rs` の排他チェック、
`src/tls_provider/libressl/` は全て削除し、rustls は aws-lc-rs（Linux/FreeBSD/macOS/
Windows）/ ring（OpenBSD/NetBSD）の無条件依存、quiche は常に同梱（Linux は共有
aws-lc-sys、それ以外は `boringssl-boring-crate`）というシンプルな構成へ巻き戻した
（F-136 の capsicum/pledge 下の証明書ホットリロードはそのまま維持）。

以下は撤回前の設計・実装記録として残す。実機で判明した知見（quiche が quictls 専用の
QUIC API（`SSL_set_quic_method` 等）を要求しバニラ OpenSSL 3.x では動かないこと、
`rustls-openssl` が OpenSSL 3.x の provider/FIPS API に依存し LibreSSL でコンパイル
できないこと、aws-lc-sys の静的ライブラリがシステム libssl/libcrypto をリンカの `-L`
探索順で遮蔽する構造的な問題など）は、将来同様の検討をする際の参考資料として価値がある。

---

## 目的

`--features system-tls` で、rustls（H1/H2 の TLS 終端）が **システムの SSL ライブラリ
（OpenSSL / LibreSSL）に動的リンク**するようにする。rustls 本体はそのまま使い、
**依存する暗号ライブラリだけ**をシステムライブラリへ差し替える。

主目的は OpenBSD packaging: OpenBSD ベースにある LibreSSL の共有ライブラリを使い、
vendored な暗号ライブラリ（aws-lc-sys / ring のビルド、quiche の vendored BoringSSL の
cmake ビルド）を避けること。設計・調査の詳細は
[docs/artifacts/f136_platform_design.md](../../artifacts/f136_platform_design.md) の
「F-137」節を参照。

## 実装内容

### rustls（H1/H2）— 全対象プラットフォームで有効

- 依存追加: `rustls-openssl = { version = "0.3.1", optional = true, default-features = false, features = ["tls12"] }`
  （`vendored` feature は有効化しない。静的埋め込みになり目的に反するため）。
- `openssl = { version = "0.10", optional = true }`: HTTP/3（quiche）の RNG
  （`SecureRandom`/`SystemRandom` 相当）シム用。`openssl::rand::rand_bytes` を薄くラップする
  `src/tls_provider.rs::system_tls_rand` モジュールを追加し、既存の呼び出し形
  （`SystemRandom::new()` + `fill(&mut buf) -> Result<(), _>`）を維持することで
  `src/http3_server.rs` は無変更。
- `src/tls_provider.rs`: `#[cfg(feature = "system-tls")] pub use rustls_openssl as provider;`
  を最優先の分岐として追加し、既存の `aws_lc_rs`/`ring` 分岐へ `not(feature = "system-tls")`
  を足す。

### HTTP/3（quiche）— OpenBSD のみ有効

**重要な決定事項（実験で確認）**: `system-tls` feature 本体には quiche の `openssl`
feature を含めない。Linux で `quiche?/openssl`（システム libssl/libcrypto へ動的リンク）を
`--features "full,system-tls"` で有効化したところ、以下のリンクエラーで失敗した:

```
undefined symbol: EVP_Q_digest / SSL_ctrl / EVP_CIPHER_CTX_dup / ERR_get_error_all ...
```

原因: Linux の quiche は既定で `aws-lc-sys`（`.cargo/config.toml` の
`AWS_LC_SYS_NO_PREFIX_x86_64_unknown_linux_gnu=1` で **非プレフィックス**ビルド、
rustls の aws_lc_rs と共有）にリンクする。非プレフィックスの aws-lc-sys は
`EVP_*`/`SSL_*` 等、**本物の OpenSSL と同名の静的シンボル**を提供する。ここへ
quiche の `openssl` feature（pkg-config 経由でシステム libssl/libcrypto を動的リンク）を
追加すると、同名シンボルを提供する 2 つのライブラリ（静的な aws-lc-sys と動的なシステム
libssl/libcrypto）が同一バイナリに同居し、mold リンカでシンボル解決に失敗する
（FreeBSD/macOS/Windows も同様に `boringssl-boring-crate` と `openssl` の同時指定で
同じクラスの問題が起きる）。

**OpenBSD だけ回避できる**: OpenBSD の quiche 依存は aws-lc-sys 共有から完全に切り離して
あるため（`rustls` は `ring`）、quiche に `openssl` feature を単独で追加しても衝突しない。
そのため `full-openbsd`/`full-openbsd-aarch64` の feature リストが `quiche?/openssl` を
直接追加している（`system-tls` 本体には含めない）。

- `Cargo.toml`: `[target.'cfg(target_os = "openbsd")'.dependencies]` の `quiche`/`boring`
  を `optional = true` にし、新設の内部フォワーディング feature `openbsd-vendor-tls`
  （`quiche?/boringssl-boring-crate` + `dep:boring`）で従来の vendored BoringSSL 構成を
  選択できるようにした（`full-openbsd-vendor`/`full-openbsd-aarch64-vendor` が使用）。
- `src/http3_server.rs`: `new_quic_config_with_certs`/`reload_quiche_certs` の cfg を
  `any(target_os = "linux", feature = "system-tls")`（memfd/一時ファイル経由のパス指定
  API）と `all(not(target_os = "linux"), not(feature = "system-tls"))`（従来の
  `with_boring_ssl_ctx_builder` in-memory API）に分割。Linux は無変更（system-tls の
  有無に関わらず同じコードパス）。OpenBSD + system-tls は前者（パス経由）を使う。
- `src/entry.rs`: FreeBSD capsicum capability mode の cap-enter 監視スレッド内で、
  `system-tls` + `http3` 有効時に「HTTP/3 の証明書ホットリロードは capability mode 下で
  機能しない」旨を起動時に警告する（quiche が内部で `fopen(3)`/`open(2)` を直接呼ぶため、
  veil 自身の dirfd 相対 openat チョークポイントで代替できない。H1/H2 は影響を受けない）。

### kTLS との併用（自動無効化）

`veil_ktls` は aws_lc_rs 固有の cipher_suite 定数を直接参照するため `system-tls`
（rustls-openssl）とは併用できない。`build.rs` が `ktls` + `system-tls` 同時指定時に
`veil_ktls` cfg を立てず（kTLS を自動的に無効化し）、`cargo:warning` を 1 行出す。

### feature セットの再編

- `full-openbsd` = 従来の内容 + `system-tls` + `quiche?/openssl`
  （既定の OpenBSD packaging 構成。LibreSSL へ動的リンク）
- `full-openbsd-vendor` = 従来の `full-openbsd` の内容そのまま
  （`openbsd-vendor-tls` で rustls+ring / quiche+BoringSSL を同梱）
- `full-openbsd-aarch64` / `full-openbsd-aarch64-vendor` も同様に追加

### packaging

`packaging/scripts/build-cross.sh`・`packaging/README.md`・`tools/qemu/bsd-vm.sh` の
OpenBSD ビルドを `full-openbsd`（= system-tls）を使うよう変更。

### OpenBSD: H3 証明書ホットリロードと unveil（レビュー指摘で追加修正）

`full-openbsd`（= `system-tls`）では `new_quic_config_with_certs` が
`cfg(any(target_os = "linux", feature = "system-tls"))` 側、つまり
`create_memfd_for_pem` によるパス経由（OpenBSD には memfd が無いため 0600 一時ファイル、
`std::env::temp_dir()` 配下、Drop で unlink）にフォールバックする。当初の実装は
`src/config.rs::collect_unveil_paths`（OpenBSD 専用の unveil 対象パス収集）に
一時ディレクトリを追加しておらず、unveil のビューに無いパスへの一時ファイル作成が失敗し、
**HTTP/3 証明書ホットリロードが恒常的に無効になる**欠陥があった（F-136 の受け入れ条件を
OpenBSD について満たさなくなる）。`collect_unveil_paths` に `#[cfg(feature = "system-tls")]`
で `std::env::temp_dir()` を `read_write_create` へ追加して修正（`pledge` の promise には
既に `wpath cpath` が含まれているため promise 側の変更は不要）。

### 構成間のトレードオフ（OpenBSD）

| 構成 | quiche の TLS | H3 証明書ホットリロード | 備考 |
|---|---|---|---|
| `full-openbsd`（既定） | システム LibreSSL へ動的リンク | 一時ファイル経由（`std::env::temp_dir()`、要 unveil 追加。上記で対応済み） | 配布サイズ小・OS の LibreSSL 更新に追従 |
| `full-openbsd-vendor` | BoringSSL 同梱（静的） | **in-memory `SSL_CTX`**（ファイル不使用、F-136 のまま） | ファイルシステムに一切触れないためサンドボックス耐性が高い |

FreeBSD（`full-freebsd`）は `system-tls` を含まないため、HTTP/3 証明書は引き続き
`with_boring_ssl_ctx_builder` による in-memory 経路であり、capsicum capability mode 下でも
証明書ホットリロードが動作する（F-136 の成果はそのまま維持される）。

## 検証

- **Linux**: `cargo tree --features full` が `system-tls` 追加前後で 1 バイトも変わらない
  ことを確認（差分は `cargo:` の "Locking packages" 系ログのみで、実ツリーは同一）。
  **`cargo build --features "full,system-tls"` は現状リンクエラーで失敗する**
  （下記「既知の限界」参照）。`http3` を外した最小構成
  （`cargo build --no-default-features --features "http2,mimalloc,system-tls"`）でも
  同じリンクエラーが再現するため、quiche の有無に関係なく **Linux では常に aws-lc-rs/
  aws-lc-sys がターゲット依存として無条件に組み込まれる**（`cargo tree --no-default-features`
  でも `aws-lc-sys` が残ることを確認済み）ことが根本原因であり、`system-tls` の
  quiche 機能フラグを触るかどうかとは無関係に失敗する。詳細は下記「既知の限界」参照。
- **OpenBSD（QEMU）**: 未実施（別途）。`full-openbsd` でビルドし `ldd` で
  `libssl.so.*`/`libcrypto.so.*` が出ること、H1/H2/H3 いずれも動作すること、証明書
  ホットリロードが機能すること（unveil 修正後）を確認する。

## 既知の限界・今後の課題

- **【未解決】Linux（および FreeBSD/macOS/Windows）で `system-tls` はリンクできない。**
  当初 quiche 側のシンボル衝突（`boringssl-boring-crate`/`aws-lc-sys` 共有 + quiche
  `openssl` feature の同時有効化）だけを問題視して OpenBSD 限定にしたが、それとは
  **別に、quiche・HTTP/3 を一切使わない構成でも同じ種類のリンクエラーになる**ことを
  検証で確認した:

  ```
  cargo build --no-default-features --features "http2,mimalloc,system-tls"
  → mold: error: undefined symbol: EVP_Q_digest / EVP_CIPHER_get_block_size / ...
  ```

  原因は、`aws-lc-rs`（したがって `aws-lc-sys`）が **Linux/FreeBSD ターゲットで
  Cargo フィーチャーに関係なく常に依存グラフに含まれる**こと（`Cargo.toml` の
  `[target.'cfg(not(target_os = "openbsd"))'.dependencies]` に非 `optional` で
  宣言されている。`cargo tree --no-default-features` でも `aws-lc-sys` が残ることを
  確認済み）。aws-lc-sys のビルド成果物ディレクトリには（`AWS_LC_SYS_NO_PREFIX` の
  値に関わらず）`libssl.a`/`libcrypto.a` という**システムの OpenSSL と同名の静的
  ライブラリ**が生成され、このディレクトリが最終リンクコマンドの `-L` に含まれる。
  `rustls-openssl`（openssl-sys 経由）が要求する `-lssl -lcrypto`（動的リンク狙い）を
  ld/mold が解決する際、**この aws-lc-sys のディレクトリが `-L` の並びで先に来るため
  そちらの静的ライブラリが優先され**、aws-lc-sys が実装していない新しめの OpenSSL 3.0
  系シンボル（`EVP_Q_digest`、`EVP_default_properties_is_fips_enabled`、
  `ERR_get_error_all` 等）が未定義エラーになる。quiche の feature 設定とは無関係に
  発生する、より根本的な問題。
  - 対策候補（いずれも本チケットの範囲を超える設計変更を要するため未着手）:
    (a) `aws-lc-rs`/`aws-lc-sys` を `optional = true` にして `system-tls` 非使用時のみ
    有効化するフィーチャー（例: `vendor-crypto`）でラップする。ただし
    「`cargo build --no-default-features`（フィーチャー一切無し）が単体でビルドできる
    こと」という既存の受け入れ条件と衝突する（`vendor-crypto` を `default`/`full` に
    含めても、`--features "full,system-tls"` のように `default` が自動有効なまま
    `system-tls` と併用される呼び出しでは両方が同時有効になり衝突が再発するため、
    Cargo のフィーチャーは加算のみで「片方が立っていたらもう片方を無効化する」という
    否定的な表現ができない）。
    (b) aws-lc-sys 側のビルド成果物のファイル名/リンク方法を変更する（aws-lc-sys 自体の
    改修が必要で veil 側からは制御できない）。
    (c) リンカ引数で `-L` の探索順序を制御し、システムの `/usr/lib/...` を
    aws-lc-sys の成果物ディレクトリより前に来るよう強制する（Cargo/rustc の
    リンク引数生成順序に依存し、フィーチャー単位で切り替えられないため
    `system-tls` 無効時に影響が出ないことを保証しにくい）。
  - **現時点の結論**: `system-tls` は **OpenBSD でのみ**動作を確認できる状態
    （OpenBSD の rustls/quiche 依存は aws-lc-sys 共有から完全に切り離してあるため、
    この衝突が発生しない）。Linux/FreeBSD/macOS/Windows での `system-tls` は
    **リンクできないため使用しないこと**。`full`/`full-freebsd` 等、`system-tls` を
    含まない既存の feature セットは本チケットによる影響を一切受けない
    （`cargo tree --features full` 差分ゼロを確認済み）。
- FreeBSD capsicum capability mode 下では（`system-tls` が使えないため関係する状況は
  今のところ無いが、将来 (a) を解消した場合は）`system-tls` + `http3` で証明書ホット
  リロードが機能しない（前述、起動時警告のみで動作は継続）。
