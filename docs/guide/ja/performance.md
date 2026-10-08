# パフォーマンスチューニングとベンチマーク

[← ドキュメント目次](README.md) · [English](../performance.md)

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

SO_REUSEPORTを使用して複数のワーカースレッドが同一ポートをリッスンする際、デフォルトではLinuxカーネルが3元タプルハッシュ（protocol + source IP + source port）で接続を振り分けます。CBPFモードでは、フローハッシュ（`skb->hash`。送信元/宛先IP・ポートの4タプルから計算）に基づいてワーカーを選択するカスタムBPFプログラムをカーネルにアタッチします（ハッシュが未計算の場合は受信CPU番号にフォールバック）。多数のクライアントからの接続を固定ワーカーへ振り分けたい場合の選択肢であり、既定は `kernel` のままです。

#### 効果

| 項目 | Kernel（デフォルト） | CBPF |
|------|---------------------|------|
| 振り分けキー | protocol + src IP + src port | フローハッシュ（4タプル）。未計算時は受信CPUへフォールバック |
| 同一接続 | 接続中は固定 | 常に同じワーカー |
| CPUキャッシュ効率 | 中 | 同等（どちらも接続を 1 ワーカーへ固定する） |
| TLSセッション再開 | 中 | 同等 |

> **注記（B-76）**: カーネル既定も 4 タプルをハッシュするため、実際には `"cbpf"` は
> `"kernel"` とほぼ同じ挙動になる。本モードの意義は「振り分けポリシーを自前の
> プログラムとして持てること」と「`skb->hash` が使えない場合に受信 CPU へ
> フォールバックできること」にある。**B-76 以前の本モードは壊れており、
> プログラムが常に 0 を返していたため全接続がワーカー 0 に固定され、
> マルチワーカーの並列性が完全に失われていた。**

#### 設定

```toml
[performance]
# "kernel" = カーネルデフォルト【既定】
# "cbpf"   = フローハッシュ（4タプル）ベースのCBPF（同一接続を固定ワーカーへ振り分け）
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

veil は起動時に `RLIMIT_NOFILE` の soft limit を hard limit まで自動で引き上げます（nginx の `worker_rlimit_nofile` 相当）。実効的なファイルディスクリプタ上限は、systemd の `LimitNOFILE` や docker の `--ulimit nofile` で hard limit を設定して制御してください。

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

### バッファサイズとタイムアウト

コード内の定数（コンパイル時に設定、再ビルドが必要）：

```rust
// バッファサイズ
const BUF_SIZE: usize = 65536;           // 64KB - io_uring最適サイズ
const HEADER_BUF_CAPACITY: usize = 512;  // HTTPヘッダー用
const MAX_HEADER_SIZE: usize = 8192;     // 8KB - ヘッダーサイズ上限
const MAX_BODY_SIZE: usize = 10485760;   // 10MB - ボディサイズ上限

// タイムアウト
const READ_TIMEOUT: Duration = Duration::from_secs(30);   // 読み込みタイムアウト
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);  // 書き込みタイムアウト
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10); // バックエンド接続タイムアウト
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);   // Keep-Aliveアイドルタイムアウト
```

> **注意**: ルートごとのセキュリティ設定で `client_header_timeout_secs` や `backend_connect_timeout_secs` を設定することで、一部のタイムアウトはconfig.tomlから個別に調整可能です。

### バッファプール設定

バッファプールは起動時にバッファを事前確保することで、メモリアロケーションのオーバーヘッドを削減します。`[buffer_pool]`セクションで設定できます：

```toml
[buffer_pool]
# 読み込みバッファサイズ（バイト）
# デフォルト: 65536 (64KB)
read_buffer_size = 65536

# 読み込みバッファ初期プール数
# デフォルト: 32
initial_read_buffers = 32

# 読み込みバッファ最大プール数
# デフォルト: 128
max_read_buffers = 128

# リクエスト構築バッファサイズ（バイト）
# デフォルト: 1024 (1KB)
request_buffer_size = 1024

# リクエスト構築バッファ初期プール数
# デフォルト: 16
initial_request_buffers = 16

# 大容量リクエストバッファサイズ（バイト）
# デフォルト: 4096 (4KB)
large_request_buffer_size = 4096

# パス文字列バッファサイズ（バイト）
# デフォルト: 256

# レスポンスヘッダーバッファサイズ（バイト）
# デフォルト: 512
```

**注意**: バッファプール設定はオプションです。デフォルト値はほとんどの用途に最適化されています。特定のメモリ制約やパフォーマンス要件がある場合のみ調整してください。

## ベンチマーク

```bash
# wrk を使用したベンチマーク
wrk -t4 -c100 -d30s https://localhost/

# kTLS有効/無効での比較

# 1. kTLS無効（rustls使用）
cargo build --release
./veil -c ./examples/config.toml &
wrk -t4 -c100 -d30s https://localhost/

# 2. kTLS有効（rustls + 独自kTLSモジュール使用）
cargo build --release --features ktls
# config.tomlでktls_enabled = true
./veil -c ./examples/config.toml &
wrk -t4 -c100 -d30s https://localhost/
```
