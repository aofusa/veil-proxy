# TLS

[← ドキュメント目次](README.md) · [English](../tls.md)

## TLS証明書の生成

開発・テスト用の自己署名証明書を生成するには、以下のコマンドを実行します：

```bash
# ECDSA秘密鍵の生成（secp384r1）
openssl genpkey -algorithm EC -out server.key -pkeyopt ec_paramgen_curve:secp384r1 -pkeyopt ec_param_enc:named_curve

# 自己署名証明書の生成（有効期限365日）
openssl req -new -x509 -key server.key -out server.crt -days 365 -subj "/CN=localhost/O=Development/C=JP"
```

生成されたファイルを `config.toml` で指定してください：

```toml
[tls]
cert_path = "./server.crt"
key_path = "./server.key"
```

> **注意**: 本番環境では、Let's Encryptなどの認証局から発行された証明書を使用してください。

## TLSライブラリ

### rustls（デフォルト）

- メモリ安全な純Rust実装
- 追加の依存関係なし
- kTLSを使用しない場合のデフォルト

### rustls + 独自kTLSモジュール（`--features ktls`）

- rustls でTLSハンドシェイクを実行
- ハンドシェイク完了後、独自のカーネルTLSモジュール（`src/ktls.rs`、`src/ktls_rustls.rs`）経由でkTLSへオフロード
- 追加の外部依存関係なし（純Rust実装）

```bash
# ビルド
cargo build --release --features ktls
```

## kTLS（Kernel TLS）サポート

### 概要

kTLSはLinuxカーネルの機能で、TLSデータ転送フェーズの暗号化/復号化をカーネルレベルで行います。
本プロジェクトでは、rustls と独自実装のカーネルTLSモジュール（`src/ktls.rs`、`src/ktls_rustls.rs`）を使用してkTLSをサポートしています。

### パフォーマンス向上

| 項目 | 効果 |
|------|------|
| CPU使用率 | 20-40%削減（高負荷時） |
| スループット | 最大2倍向上 |
| レイテンシ | コンテキストスイッチ削減 |
| ゼロコピー | sendfile + TLS暗号化 |

### 有効化手順

```bash
# 1. カーネルモジュールのロード
sudo modprobe tls

# 2. ktlsフィーチャー付きでビルド
cargo build --release --features ktls

# 3. 設定ファイルで有効化（config.toml）
# [tls]
# ktls_enabled = true
# ktls_fallback_enabled = true  # オプション
```

### フォールバック設定

kTLSの有効化に失敗した場合の動作を `ktls_fallback_enabled` で制御できます：

| 設定値 | 動作 |
|--------|------|
| `true`（デフォルト） | kTLS失敗時はrustlsで継続（graceful degradation） |
| `false` | kTLS必須モード（失敗時は接続拒否） |

**フォールバック無効化 (`ktls_fallback_enabled = false`) のメリット:**

| 観点 | 効果 |
|------|------|
| パフォーマンス予測可能性 | すべての接続が確実にkTLSを使用 |
| デバッグ容易性 | kTLS/rustls混在状態がなくなる |
| 環境問題の早期発見 | kTLS利用不可時に即座に失敗 |

**注意:** フォールバック無効時は、kTLSが利用できない環境で接続が失敗します。
事前に `modprobe tls` でカーネルモジュールがロードされていることを確認してください。

```toml
[tls]
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"
ktls_enabled = true
ktls_fallback_enabled = false  # kTLS必須モード
```

### 要件

- Linux 5.15以上（推奨、5.15未満でも動作可能）
- `tls`カーネルモジュールがロード済み
- AES-GCM暗号スイート（TLS 1.2/1.3）
- ktlsフィーチャーでビルド（`--features ktls`）

### 実装状況

**ktlsフィーチャー有効時（`--features ktls`）:**
- ✅ kTLSカーネルモジュールの可用性チェック
- ✅ TLSハンドシェイク完了後の自動kTLS有効化
- ✅ 送信（TX）と受信（RX）の両方でkTLSオフロード
- ✅ monoio (io_uring) との完全な非同期統合

**デフォルトビルド（rustls使用）:**
- ❌ kTLSはサポートされていない
- 👉 kTLSを使用するには `--features ktls` でビルドしてください

### セキュリティ考慮事項

| リスク | 緩和策 |
|--------|--------|
| カーネルバグ | カーネルバージョン固定、定期的なパッチ適用 |
| セッションキー露出 | TLSハンドシェイクはユーザースペース（rustls）で実行（PFS維持） |
| DoS攻撃 | カーネルリソース監視、レート制限 |

## TLS証明書ホットリロード

プロキシを再起動せずにTLS証明書をローテーションします。

### 動作原理

- バックグラウンドスレッドが `reload_interval_secs` 秒ごとに証明書ファイルの `mtime` を監視。
- 変化を検出すると新しい証明書を `ArcSwap` にロード。
- **既存のTLS接続**は旧証明書を使い続ける（接続断なし）。
- **新しいTLSハンドシェイク**は自動的に新証明書を使用。
- SIGHUPシグナルでも設定リロードと同時に即時更新。
- **HTTP/1.1・HTTP/2 に加え、HTTP/3（QUIC/quiche）もホットリロード対応**（F-105）。HTTP/3 は各ワーカーが自身の `quiche::Config` を保持するため、リロードスレッドが cert/key の生 PEM を `ArcSwap` でアトミックに配信し、各ワーカーがイベントループ先頭の安価な世代ゲート（差分検知時のみ）で反映する。**Linux** は `memfd` 経由（Landlock 互換・FS 非経由）に差し替える。**FreeBSD/OpenBSD/macOS/Windows**（F-136）は `Config::with_boring_ssl_ctx_builder` で `quiche::Config` を丸ごと in-memory 再構築し、PEM バイト列を直接 `boring::ssl::SslContextBuilder` へ渡す（ファイル・パス・memfd を一切介さない）。これにより FreeBSD capsicum capability mode 下（パス指定の `open`/`stat` が `ECAPMODE` になる環境）でも証明書リロードが継続動作する。既存 QUIC 接続は影響を受けず、新規ハンドシェイクのみ新証明書を提示する。全ワーカーの適用完了後、秘密鍵の平文はメモリ上でゼロ化（`secure_zero`）される。
- **capability mode 下の H1/H2 リロード**（F-136）: FreeBSD capsicum capability mode では、rustls `ServerConfig` 用の cert/key 読み取り（mtime 検査 + PEM 読み込み）を単一チョークポイント `tls_reload::pem_mtime`/`read_pem` に集約し、`cap_enter` 前に確保した dirfd（`security::capsicum::init_tls_cert_dirfds`）経由の `openat`/`fstatat`（`O_RESOLVE_BENEATH`）へ切り替える。Linux/macOS/Windows/OpenBSD は挙動不変（引き続き `std::fs`）。

### 設定

```toml
[tls]
cert_path = "/etc/veil/cert.pem"
key_path  = "/etc/veil/key.pem"
# ゼロダウンタイム証明書ホットリロード
auto_reload = true
reload_interval_secs = 60  # ポーリング間隔（デフォルト: 60秒）
```

### Let's Encryptとの連携

```bash
# 証明書更新（certbot）
certbot renew --deploy-hook "touch /etc/veil/cert.pem"
# veilがmtimeの変化を検知して自動リロード
```

> **注意**: Landlockサンドボックスが有効な場合、証明書ディレクトリを `landlock_read_paths` に含める必要があります。
