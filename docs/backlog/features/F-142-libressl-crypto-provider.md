# F-142: LibreSSL 対応 rustls CryptoProvider の自作

## 取り下げ（Withdrawn、2026-07-30）

**本チケットの実装はユーザー判断により F-137 と合わせて撤回済み。**
`src/tls_provider/libressl/` は削除し、`src/tls_provider/mod.rs` は単一ファイル
`src/tls_provider.rs` に戻した。`vendored-tls` feature（同梱 aws-lc-rs/aws-lc-sys を
optional 化する仕組み）も削除し、rustls の暗号プロバイダは再び target_os のみで決まる
無条件依存（F-122 の状態）に戻している。

以下は撤回前の設計・実装記録として残す。`rustls-openssl` crate が OpenSSL 3.x の
provider/FIPS API に依存し LibreSSL でコンパイルできないという実機知見（NetBSD で
確認）、および自作 `CryptoProvider` の実装方針（`openssl` クレートのクラシック API
のみを使う設計）は、将来同様の検討をする際の参考資料として価値がある。

---

## 目的

`system-tls`（F-137/F-140）は当初 `rustls-openssl` 0.3.1 crate を使う設計だったが、
**NetBSD 実機ビルドで `rustls-openssl` が LibreSSL 上でコンパイルできない**ことが判明した
（`cannot find provider in openssl` / `EVP_default_properties_enable_fips` 等、
OpenSSL 3.x の provider/FIPS API に依存しているため）。

quiche（HTTP/3）は BoringSSL 系 QUIC API を要求し、OpenBSD/NetBSD で選べるシステム SSL は
LibreSSL のみ。rustls 側が OpenSSL 3.x 専用 API に依存する `rustls-openssl` のままでは
OpenBSD/NetBSD の `system-tls` が成立しないため、**`openssl` クレートの「クラシック API」
だけを使って rustls の `CryptoProvider` を自作した**。crates.io に LibreSSL ベースの
rustls provider crate が存在しないことは確認済み。

設計の詳細は
[docs/artifacts/f142_libressl_provider_design.md](../../artifacts/f142_libressl_provider_design.md)
を参照。

## 改修内容

- `src/tls_provider.rs` を `src/tls_provider/mod.rs` へ移動し、`src/tls_provider/libressl/`
  を新設（`#[cfg(feature = "system-tls")]` 配下のみコンパイル）。
- 実装したトレイト（すべて `openssl` クレートのクラシック API のみ使用。
  OpenSSL 3.x 専用の provider/FIPS/`rand_priv_bytes`/`Id::ED448` は一切使わない）:
  - `SecureRandom`（`openssl::rand::rand_bytes`）
  - `hash::Hash` / `hmac::Hmac`（SHA-256/384、`openssl::sha`/`openssl::sign::Signer`）。
    TLS1.3 の HKDF は自前実装せず `rustls::crypto::tls13::HkdfUsingHmac` に、
    TLS1.2 の PRF も `rustls::crypto::tls12::PrfUsingHmac` に、上記 `Hmac` を渡すだけで
    賄う（自前で HKDF/PRF を書かない）。
  - AEAD（`Tls13AeadAlgorithm`/`Tls12AeadAlgorithm`）: AES-128/256-GCM、
    ChaCha20-Poly1305。**ホットパス対応**として `openssl::cipher_ctx::CipherCtx` を
    コネクション確立時に一度だけ確保し、レコードごとに `encrypt_init`/`decrypt_init` で
    再初期化して使い回す（`CipherCtx::new()` を毎回呼ばない）。
  - 鍵交換（`SupportedKxGroup`）: X25519 → secp256r1 → secp384r1。
  - 署名検証（`SignatureVerificationAlgorithm`）: ECDSA P-256/P-384、RSA PKCS#1、
    RSA PSS、Ed25519。**Ed448 は LibreSSL 非対応のため実装しない。**
  - 秘密鍵（`KeyProvider`/`SigningKey`/`Signer`）: PKCS#8 DER から ECDSA/RSA/Ed25519 鍵を
    読み込み。
- `rustls-openssl` への依存を削除（`openssl` クレートは引き続き使用）。
- ユニットテスト（`cargo test`、各モジュールの `#[cfg(test)]`）:
  AEAD 暗号化→復号ラウンドトリップ（タグ改竄検出・`CipherCtx` 再利用込み）、
  HMAC-SHA256 の RFC 4231 既知ベクタ、ECDSA/RSA/Ed25519 の署名→検証ラウンドトリップ、
  X25519/secp256r1/secp384r1 の共有秘密一致。

## 検証

- ローカル（Linux）: `build.rs` の `SYSTEM_TLS_ALLOWED_TARGET_OSES` に一時的に
  `"linux"` を加えてコンパイル・ユニットテストを実行（Linux は OpenSSL 3.x のため
  LibreSSL 固有部分の実行時検証はできないが、ロジックは検証済み）。
  **F-142 の追加タスクで `system-tls` が Linux/FreeBSD でも正式に使えるようになった
  ため、この一時変更は最終的に恒久化されている**（下記「関連: system-tls の
  全 Unix 対応」参照）。
- 実機（OpenBSD/NetBSD）検証はコーディネーターが QEMU で実施。

## 関連: `system-tls` の全 Unix 対応 + `vendored-tls` feature 新設

同チケットの追加タスクとして、`system-tls` を Linux/FreeBSD にも拡大した。

- `vendored-tls` feature を新設: 同梱 `aws-lc-rs`/`aws-lc-sys`（rustls の `aws_lc_rs`
  cargo feature 経由）を `optional = true` 化し、この feature 配下にまとめた。
  `default`/`full`/`full-freebsd`/`full-freebsd-aarch64` に追加し、
  **既定ビルドの依存グラフ・挙動は不変**（`cargo tree --features full` 差分ゼロを確認）。
  OpenBSD/NetBSD の `ring` はシステム libssl との静的/動的リンク衝突を起こさないため
  対象外とし、`full-openbsd(-vendor)`/`full-netbsd(-vendor)` は変更していない
  （安全側の判断。詳細はコミットログ・作業報告を参照）。
- `build.rs`: `SYSTEM_TLS_ALLOWED_TARGET_OSES` に `linux`/`freebsd` を追加。
  `vendored-tls` と `system-tls` の同時指定は明確なエラーで停止する。
- `full-system-tls`（Linux 汎用）/ `full-freebsd-system-tls` を新設。
  **FreeBSD 版は `http3` を含まない**: FreeBSD の HTTP/3 証明書ホットリロード
  （F-136）は vendored BoringSSL の in-memory `SSL_CTX` API
  （`boringssl-boring-crate` 限定）に直接依存しており、`system-tls` の動的リンク方針とは
  非互換なため（`src/http3_server.rs` は本チケットの対象外）。
- `system-tls` + `http3` の QUIC API 制約: quiche は BoringSSL 系 QUIC API
  （`SSL_set_quic_method` 等）を要求するが、バニラ OpenSSL 3.x にはこれが無い
  （LibreSSL 3.6+ / quictls には有る）。`build.rs::check_system_tls_quic_capability` が
  pkg-config 経由で `openssl/ssl.h` を検査し、`SSL_set_quic_method` が無ければビルド
  開始時点で明確なエラーを出す（Linux のバニラ OpenSSL 3.x では意図的に失敗する）。

| OS | system-tls での rustls | system-tls での quiche(HTTP/3) |
|---|---|---|
| Linux | ✅ OpenSSL 3.x でも LibreSSL でも可 | ⚠️ QUIC 対応 libssl（LibreSSL 3.6+ / quictls）が必要（pkg-config 検査あり） |
| FreeBSD | ✅ OpenSSL 3.x でも LibreSSL でも可 | ❌ 未対応（F-136 の in-memory 証明書リロードが boring 依存のため） |
| OpenBSD | ✅ base の LibreSSL | ✅ base の LibreSSL |
| NetBSD | ✅ pkgsrc の LibreSSL（pkgconf 必須） | ✅ pkgsrc の LibreSSL（base の OpenSSL 3.0.12 では不可） |
