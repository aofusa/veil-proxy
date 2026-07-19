# F-126: FreeBSD kTLS 送受信オフロード対応

- 優先度: P2
- ステータス: **完了**
- 起点: v0.6.0 マルチプラットフォーム対応（F-120 で FreeBSD は kTLS 非対応のまま出荷されていた）
- 設計: `docs/artifacts/f126_freebsd_ktls_design.md`（設計: Fable / 実装: Sonnet）

## 目的

FreeBSD（13.0+、14.x で検証）で kTLS 送受信オフロードに対応する。F-120 時点では
FreeBSD は `[security] enable_capsicum` 等のネイティブセキュリティのみ対応し、
kTLS は非対応（常にユーザ空間 rustls）だった。FreeBSD は Linux と並び kTLS を
早期から実装した OS であり、既存 `ktls` feature をそのまま流用して対応する。

## 改修内容

### Linux/FreeBSD の API 差分

| ステップ | Linux | FreeBSD |
|----------|-------|---------|
| ULP 設定 | `setsockopt(IPPROTO_TCP, TCP_ULP, "tls")` | 不要（no-op） |
| TX 有効化 | `setsockopt(SOL_TLS, TLS_TX, &tls12_crypto_info)` | `setsockopt(IPPROTO_TCP, TCP_TXTLS_ENABLE(39), &tls_enable)` |
| RX 有効化 | `setsockopt(SOL_TLS, TLS_RX, &tls12_crypto_info)` | `setsockopt(IPPROTO_TCP, TCP_RXTLS_ENABLE(41), &tls_enable)` |
| 構造体 | `struct tls12_crypto_info_aes_gcm_{128,256}` | `struct tls_enable`（ポインタ渡し） |

### 実装（Linux 非干渉を最優先、`target_os` cfg で完全分離）

- **`build.rs`**: `veil_ktls` cfg の発行条件を `ktls && target_os == "linux"` から
  `ktls && (target_os == "linux" || target_os == "freebsd")` へ拡張。cfg 対応表も更新。
- **`src/lib.rs`**: `#[cfg(all(veil_ktls, target_os = "freebsd"))] pub mod ktls_freebsd;` を追加。
- **`src/ktls.rs`**（Linux 専用コードは無変更・追加のみ）:
  - `setup_ulp` を `#[cfg(target_os = "linux")]`（既存実装そのまま）と
    `#[cfg(target_os = "freebsd")]`（no-op、`Ok(())`）に分離。
  - FreeBSD 向けに `TlsKeyMaterial`（固定長・ヒープ確保なしの生鍵マテリアル。
    key/salt/iv/rec_seq/version）と `extract_single_material`/`extract_tx_rx_material`
    を追加（`#[cfg(target_os = "freebsd")]`）。Linux の `CryptoInfo`/`extract_tx_rx`は
    そのまま。
- **`src/ktls_freebsd.rs`（新規）**:
  - `#[repr(C)] struct TlsEnable`（FreeBSD 14.3 実機ヘッダ準拠。下記参照）。
  - `enable_tx`/`enable_rx`: `TlsKeyMaterial` から `TlsEnable` を構築し
    `setsockopt(IPPROTO_TCP, TCP_TXTLS_ENABLE/TCP_RXTLS_ENABLE, ...)`。
    TLS 1.2 は `iv` = salt(4B) のみ、TLS 1.3 は `iv` = salt++iv 連結（12B）。
  - `is_ktls_available`（`src/ktls_rustls.rs` 側）は `kern.ipc.tls.enable` sysctl を参照。
- **`src/ktls_rustls.rs`**: `setup_ktls_after_ulp` を `target_os` で分岐（Linux は既存の
  `CryptoInfo`/`setup_tls_info` 経路、FreeBSD は `extract_tx_rx_material` +
  `ktls_freebsd::enable_tx/enable_rx`）。`is_ktls_available` も Linux（/proc）/FreeBSD
  （sysctl）で分岐。`set_tcp_cork` は FreeBSD では使用しない（no-op、保守的に無効化）。

### `struct tls_enable` の ABI（FreeBSD 14.3-RELEASE 実機で確認済み、設計時の目安値から訂正）

FreeBSD 14.3 VM（`/usr/include/sys/ktls.h` 等）で実機確認した値を SSOT として採用:

```c
struct tls_enable {
    const uint8_t *cipher_key;
    const uint8_t *iv;
    const uint8_t *auth_key;
    int    cipher_algorithm;   /* CRYPTO_AES_NIST_GCM_16 = 25 (設計時の目安値 26 は誤りだった) */
    int    cipher_key_len;
    int    iv_len;
    int    auth_algorithm;     /* AEAD では 0 */
    int    auth_key_len;       /* AEAD では 0 */
    int    flags;              /* 0 */
    uint8_t tls_vmajor;        /* TLS_MAJOR_VER_ONE = 3 */
    uint8_t tls_vminor;        /* 1.2: TLS_MINOR_VER_TWO=3 / 1.3: THREE=4 */
    uint8_t rec_seq[8];
};
```

`size_of::<TlsEnable>() == 64`、`align_of == 8`。各フィールドオフセットは
`cc`で生成した実機バイナリの`offsetof`と完全一致することを確認し、
`src/ktls_freebsd.rs::tests::test_tls_enable_layout` でコンパイル時（`offset_of!`）に
固定した。`TCP_TXTLS_ENABLE=39`/`TCP_RXTLS_ENABLE=41`/`TLS_MAJOR_VER_ONE=3`/
`TLS_MINOR_VER_TWO=3`/`TLS_MINOR_VER_THREE=4` は設計ドキュメントの目安値と一致。
`CRYPTO_AES_NIST_GCM_16` のみ実機値が 25（`/usr/include/crypto/cryptodev.h`）で
設計時の目安値 26 と異なっていたため実機値を採用。

## ゼロコピー転送（sendfile / splice）のプラットフォーム分岐

kTLS 有効化後のデータ転送最適化は OS ごとに可否が異なるため、`veil_ktls` かつ
`target_os` で分岐する:

- **sendfile(2)**: FreeBSD にも存在し（発祥）、**kTLS 対応**（ソケットが kTLS 有効なら
  カーネル内で暗号化して送信）。ただし Linux とは引数順・戻り値が異なる 7 引数 API
  （`sendfile(file_fd, socket_fd, offset, nbytes, hdtr, sbytes, flags)`、送信バイト数は
  `sbytes` OUT）。`src/ktls_rustls.rs::sendfile_ktls` を `#[cfg(target_os = "linux")]` /
  `#[cfg(target_os = "freebsd")]` の 2 実装に分け、静的ファイル配信のゼロコピー送信を
  **FreeBSD でも有効**にした（F-126、ユーザ要望により実装）。
- **splice(2)**: Linux 専用で FreeBSD に等価物が無い。splice ベースのプロキシボディ転送
  （`proxy.rs::splice_body_transfer`/`proxy_http_request_splice`/
  `splice_transfer_response_ktls`）と splice 用パイプ管理（`SplicePipe`/`set_pipe_size`＝
  `F_SETPIPE_SZ`/`pool.rs` の splice パイププール）は `#[cfg(all(veil_ktls,
  target_os = "linux"))]` に限定。FreeBSD は `try_splice_proxy` が `None` を返し、
  通常のバッファ経由 read/write 転送（`proxy_http_request_with_compression`）へ
  フォールバックする（kTLS のカーネル暗号化自体は有効なまま）。

## FreeBSD VM 実機検証（14.3-RELEASE）

- ビルド: FreeBSD 14.3 VM 上で `--features ktls` ネイティブビルド（warning 0 を確認）。
- `sysctl kern.ipc.tls.enable=1` 設定後、静的配信 HTTPS E2E で 200 + 本文一致を確認。
- kTLS 有効化を `sysctl kern.ipc.tls.stats` のカウンタ増加で確認。
- `sysctl kern.ipc.tls.enable=0`（既定値）でも rustls へフォールバックし機能継続を確認
  （`setsockopt` が失敗 → `ktls_fallback_enabled` 経由でグレースフル劣化）。
- 複数 TLS レコードにまたがる大きめボディ（sendfile 経路）でも取りこぼしなく応答を確認。

> 注: 上記実機検証は本ブランチ（L4 UDP / macOS 対応を含む v0.6.0 系）でのビルド成功後に
> Fable が FreeBSD 14.3 VM 上で実施する。ビルドは veil_ktls 有効化で初めて FreeBSD で
> コンパイルされる Linux 専用ゼロコピー経路（splice/F_SETPIPE_SZ/Linux sendfile）の
> cfg 分岐修正を含む。

## 受け入れ条件

- Linux io_uring/epoll の kTLS 経路（`src/ktls.rs` Linux 実装）に挙動変更なし
  （`cargo build --features full` / `cargo clippy --all-targets` / `cargo fmt --check`
  が warning/error 0）。
- OpenBSD は従来どおり kTLS 非対応（`veil_ktls` は立たず simple_tls フォールバック）。
- FreeBSD 実機で kTLS 有効化・フォールバック・複数レコードボディの応答を確認。

## 依存・リスク

- `struct tls_enable` の ABI は FreeBSD バージョン間で差異があり得る
  （13 系との差異は未検証、14.3 のみ実機確認）。将来バージョンで再確認が必要。
- FreeBSD の RX kTLS はレコード境界の扱いに注意が必要だが、既存の
  `drain_rustls_plaintext` ドレイン機構で吸収される設計を流用しており追加変更なし。
