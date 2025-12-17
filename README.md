# High-Performance Reverse Proxy Server

io_uring (monoio) と rustls を使用した高性能リバースプロキシサーバー。

## 特徴

- **非同期I/O**: monoio (io_uring) による効率的なI/O処理
- **TLS**: rustls によるメモリ安全な Pure Rust TLS実装
- **kTLS**: rustls + ktls2 によるカーネルTLSオフロード対応（Linux 5.15+）
- **高速アロケータ**: mimalloc による高速メモリ割り当て + Huge Pages対応
- **高速ルーティング**: Radix Tree (matchit) によるO(log n)パスマッチング
- **コネクションプール**: バックエンド接続の再利用によるレイテンシ削減
- **バッファプール**: メモリアロケーションの削減
- **Keep-Alive**: HTTP/1.1 Keep-Alive完全サポート
- **Chunked転送**: RFC 7230準拠のChunkedデコーダ
- **CPUアフィニティ**: ワーカースレッドのCPUコアピン留め
- **CBPF振り分け**: SO_REUSEPORTのクライアントIPベースロードバランシング（Linux 4.6+）

## ビルド

```bash
# 通常ビルド（rustls使用）
cargo build --release

# kTLSサポート付きビルド（rustls + ktls2）
cargo build --release --features ktls
```

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

### rustls + ktls2（`--features ktls`）

- rustls でTLSハンドシェイクを実行
- ハンドシェイク完了後、ktls2 経由でkTLSへオフロード
- 追加の外部依存関係なし（純Rust実装）

```bash
# ビルド
cargo build --release --features ktls
```

## 設定

`config.toml`:

```toml
[server]
listen = "0.0.0.0:443"
# ワーカースレッド数（オプション）
# 未指定または0の場合はCPUコア数と同じスレッド数を使用
threads = 4

[performance]
# SO_REUSEPORT の振り分け方式
# "kernel" = カーネルデフォルト（3元タプルハッシュ）
# "cbpf"   = クライアントIPベースのCBPF（キャッシュ効率向上、Linux 4.6+必須）
reuseport_balancing = "cbpf"

# Huge Pages (Large OS Pages) の使用
# TLBミス削減により5-10%のパフォーマンス向上
huge_pages_enabled = true

[tls]
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"
ktls_enabled = true         # kTLS有効化（Linux 5.15+、feature flag必須）
ktls_fallback_enabled = true # kTLS失敗時のrustlsフォールバック（デフォルト: true）

# ホストベースルーティング
[host_routes]
"example.com" = { type = "File", path = "/var/www/example", mode = "sendfile" }
"api.example.com" = { type = "Proxy", url = "http://localhost:8080" }

# パスベースルーティング
[path_routes."example.com"]
"/api/" = { type = "Proxy", url = "http://localhost:8080" }
"/static/" = { type = "File", path = "/var/www/static", mode = "sendfile" }
```

## ルーティング

### ルーティングの優先順位

1. **ホストベースルーティング** (`[host_routes]`): Hostヘッダーで完全一致
2. **パスベースルーティング** (`[path_routes."hostname"]`): パスの最長一致（Radix Tree）

### バックエンドタイプ

| タイプ | 説明 | 設定例 |
|--------|------|--------|
| `Proxy` | HTTPリバースプロキシ | `{ type = "Proxy", url = "http://localhost:8080" }` |
| `File` | 静的ファイル配信 | `{ type = "File", path = "/var/www", mode = "sendfile" }` |

### ファイル配信モード

| モード | 説明 | 用途 |
|--------|------|------|
| `sendfile` | sendfileシステムコールでゼロコピー送信 | 大きなファイル、動画、画像 |
| `memory` | ファイルをメモリに読み込んで配信 | 小さなファイル、favicon.ico等 |

```toml
# ディレクトリ配信（sendfileモード）
"/static/" = { type = "File", path = "/var/www/static", mode = "sendfile" }

# 単一ファイル配信（memoryモード）
"/favicon.ico" = { type = "File", path = "/var/www/favicon.ico", mode = "memory" }

# typeとmodeを省略した場合のデフォルト
"/" = { path = "/var/www/html" }  # type = "File", mode = "sendfile"
```

### プロキシ設定

HTTPおよびHTTPSバックエンドへのプロキシに対応：

```toml
# HTTPバックエンド
"/api/" = { type = "Proxy", url = "http://localhost:8080" }

# HTTPSバックエンド（TLSクライアント接続）
"/secure/" = { type = "Proxy", url = "https://backend.example.com" }
```

## kTLS（Kernel TLS）サポート

### 概要

kTLSはLinuxカーネルの機能で、TLSデータ転送フェーズの暗号化/復号化をカーネルレベルで行います。
本プロジェクトでは、rustls + ktls2 を使用してkTLSをサポートしています。

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

## パフォーマンスチューニング

### ワーカースレッド数

ワーカースレッド数は `config.toml` の `[server]` セクションで設定できます。

```toml
[server]
listen = "0.0.0.0:443"
threads = 0  # 未指定または0の場合はCPUコア数と同じ
```

| 設定 | 動作 |
|------|------|
| 未指定 | CPUコア数と同じスレッド数 |
| `threads = 0` | CPUコア数と同じスレッド数 |
| `threads = 4` | 4スレッドで起動 |

- 各ワーカースレッドはCPUコアにピン留めされます（CPUアフィニティ）
- コア数よりスレッド数が多い場合はラウンドロビンで割り当て
- メモリ制約がある環境では少なめに設定することを推奨

### SO_REUSEPORT CBPFロードバランシング

#### 概要

SO_REUSEPORTを使用して複数のワーカースレッドが同一ポートをリッスンする際、デフォルトではLinuxカーネルが3元タプルハッシュ（protocol + source IP + source port）で接続を振り分けます。CBPFモードでは、クライアントIPアドレスのみに基づいてワーカーを選択するカスタムBPFプログラムをカーネルにアタッチします。

#### 効果

| 項目 | Kernel（デフォルト） | CBPF |
|------|---------------------|------|
| 振り分けキー | protocol + src IP + src port | src IP のみ |
| 同一クライアント | source portで変動 | 常に同じワーカー |
| CPUキャッシュ効率 | 中 | 高（L1/L2ヒット率向上） |
| TLSセッション再開 | 低〜中 | 高（セッションキャッシュ活用） |

#### 設定

```toml
[performance]
# "kernel" = カーネルデフォルト（後方互換性）
# "cbpf"   = クライアントIPベースのCBPF（推奨）
reuseport_balancing = "cbpf"
```

#### 要件

- **Linux 4.6以上**（SO_ATTACH_REUSEPORT_CBPFサポート）
- CBPFアタッチ失敗時は自動的にカーネルデフォルトにフォールバック

### Huge Pages（Large OS Pages）

#### 概要

mimallocアロケータでHuge Pages（2MB）を使用することで、TLB（Translation Lookaside Buffer）ミスを削減し、パフォーマンスを向上させます。

#### 効果

| 項目 | 効果 |
|------|------|
| TLBミス | 大幅削減（ページテーブル参照の減少） |
| ページフォルト | 大容量メモリ使用時に減少 |
| パフォーマンス | 5-10%向上（ワークロード依存） |
| kTLS/splice | カーネル連携時に特に効果的 |

#### 設定

```toml
[performance]
huge_pages_enabled = true
```

#### OSレベルの設定（Linux）

```bash
# 一時的にHuge Pagesを有効化（128ページ = 256MB）
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# 永続化（/etc/sysctl.conf）
echo "vm.nr_hugepages=128" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p

# 現在のHuge Pages状態を確認
grep -i huge /proc/meminfo
```

#### コンテナ環境での注意

Docker/Kubernetes環境では、ホスト側でHuge Pagesを事前に確保する必要があります：

```bash
# ホスト側でHuge Pagesを確保
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# Docker起動時（オプション）
docker run --shm-size=256m ...

# Kubernetes（Pod仕様に追加）
# resources.limits.hugepages-2Mi: "256Mi"
```

Huge Pagesが利用できない場合は、自動的に通常の4KBページにフォールバックします。

### システム設定

```bash
# ファイルディスクリプタ上限
ulimit -n 65535

# カーネルパラメータ
sysctl -w net.core.somaxconn=65535
sysctl -w net.ipv4.tcp_max_syn_backlog=65535
sysctl -w net.core.netdev_max_backlog=65535

# io_uringの設定（必要に応じて）
sysctl -w kernel.io_uring_setup_flags=0
```

### バッファサイズ

コード内の定数を調整可能：

```rust
const BUF_SIZE: usize = 65536;           // 64KB - io_uring最適サイズ
const HEADER_BUF_CAPACITY: usize = 512;  // HTTPヘッダー用
const MAX_HEADER_SIZE: usize = 8192;     // 8KB - ヘッダーサイズ上限
const MAX_BODY_SIZE: usize = 10485760;   // 10MB - ボディサイズ上限
```

## ベンチマーク

```bash
# wrk を使用したベンチマーク
wrk -t4 -c100 -d30s https://localhost/

# kTLS有効/無効での比較

# 1. kTLS無効（rustls使用）
cargo build --release
./target/release/zerocopy-server &
wrk -t4 -c100 -d30s https://localhost/

# 2. kTLS有効（rustls + ktls2使用）
cargo build --release --features ktls
# config.tomlでktls_enabled = true
./target/release/zerocopy-server &
wrk -t4 -c100 -d30s https://localhost/
```

## 参考資料

### コアライブラリ

- [monoio](https://github.com/bytedance/monoio): io_uringベースの非同期ランタイム
- [rustls](https://github.com/rustls/rustls): Pure Rust TLS実装
- [ktls2](https://crates.io/crates/ktls2): rustls用kTLS統合クレート

### パフォーマンス

- [mimalloc](https://github.com/microsoft/mimalloc): 高速汎用メモリアロケータ
- [matchit](https://crates.io/crates/matchit): 高速Radix Treeルーター
- [Linux Huge Pages](https://docs.kernel.org/admin-guide/mm/hugetlbpage.html): Large OS Pages設定ガイド

### カーネル機能

- [Linux Kernel TLS](https://docs.kernel.org/networking/tls.html): kTLSドキュメント
- [io_uring](https://kernel.dk/io_uring.pdf): io_uring設計ドキュメント
- [SO_REUSEPORT](https://lwn.net/Articles/542629/): ポート共有とロードバランシング

## ライセンス

Apache License 2.0

(c) 2025 aofusa
