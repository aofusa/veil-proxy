# セキュリティとサンドボックス

[← ドキュメント目次](README.md) · [English](../security.md)

## 自己サンドボックス化（Self-Sandboxing）

このサーバーは、bubblewrapなどの外部ツールを使用せずに、**コード内から自己隔離**する機能を内蔵しています。

### なぜ外部ツールではなくコード内実装か？

| 方式 | メリット | デメリット |
|------|---------|-----------|
| bubblewrap (外部) | 柔軟な設定、既存ツール | 追加の依存、設定の複雑さ |
| **本サーバー (内蔵)** | ゼロ依存、コードで宣言、自動継承 | Linuxカーネル依存 |

### 実装済みの自己隔離機能

#### 1. Landlock ファイルシステム制限 (Linux 5.13+)

プロセスが「これ以降、このディレクトリ以外は見ません」と宣言できます。

```toml
[security]
enable_landlock = true
landlock_read_paths = ["/etc/veil", "/usr", "/lib", "/lib64"]
landlock_write_paths = ["/var/log/veil"]
```

**対応ABIバージョン:**

| ABI | カーネル | 機能 |
|-----|---------|------|
| v1 | 5.13+ | 基本的なファイルシステムアクセス制御 |
| v2 | 5.19+ | ファイル参照権限 (REFER) |
| v3 | 6.2+ | TRUNCATE権限 |
| v4 | 6.7+ | ioctl権限 |

#### 2. seccomp システムコール制限

許可リストに基づいてシステムコールを制限します。

```toml
[security]
enable_seccomp = true
seccomp_mode = "filter"  # "log" / "filter" / "strict"
```

> 許可リストには静的ファイル配信に必要な `faccessat2`（glibc 2.33+ / musl が
> `access()`/`faccessat()` の実体として発行）を含みます。これが無いと seccomp 有効時に
> ファイル解決が `EPERM` で失敗し、静的配信が 404 になります（コンテナ用
> `docker/assets/security/seccomp.json` にも同システムコールを含めています）。
> 静的配信のパス解決（`RESOLVE_BENEATH` での封じ込め、F-153）に使う `openat2` も含みます。
> コンテナ用のプロファイルには v0.8.0 で足しました（無いと毎回 `canonicalize()` の経路に戻ります。
> 以前は起動後最初の静的配信の要求だけが 404 になっていました。B-154）。
>
> 静的配信のキャッシュの `watch = true`（[圧縮・キャッシュ](compression-cache.md#静的配信のキャッシュがディスクの変更に追従する)）の
> ために、許可リストと両方のコンテナ用のプロファイルに `inotify_init1`・`inotify_add_watch`・`inotify_rm_watch` を
> 含みます。プロセス内の許可リストには `getdents64` も足しました（監視のスレッドが静的配信のルートを辿る）。
> これらを出すのは監視の専用スレッドだけで、起動時・リロード時・ディレクトリが作られたときに限ります。

**推奨導入手順:**

```bash
# 1. まずログモードで動作確認
enable_seccomp = true
seccomp_mode = "log"

# 2. ブロックされるシステムコールを確認
journalctl -f | grep -i seccomp

# 3. 問題なければfilterモードに変更
seccomp_mode = "filter"
```

#### 3. 権限降格 (Privilege Dropping)

root起動後、リスナー作成後に非特権ユーザーへ降格します。

```toml
[security]
drop_privileges_user = "veil"
drop_privileges_group = "veil"
```

### Namespace隔離について

> **注意**: `unshare(CLONE_NEWNET)` などのNamespace隔離は、リバースプロキシでは**非推奨**です。
> ネットワーク名前空間を分離するとプロキシ機能が失われます。
> 
> Namespace隔離が必要な場合は、**systemdレベル**で行うことを推奨します（下記参照）。

## JWT の認証（F-220）

`[route.jwt]` はルートの要求ごとに JSON Web Token を検証してから action へ渡す（`jwt` feature が必要で `full*` に含まれる）。
トークンの無い要求は `401`（`Jwt is missing`）、検証できないトークンは `401`（`Jwt verification fails`）、検証できたが
`require_claims` を満たさないトークンは `403`（`RBAC: access denied`）。

```toml
[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "api"
[route.jwt]
issuer = "https://auth.example.com"       # "iss" と照合（空なら見ない）
audiences = ["api"]                       # どれかが "aud" に入っていること（空なら見ない）
jwks_path = "/etc/veil/jwks.json"         # または jwks = '{"keys":[...]}' でインライン
# header = "authorization"                # 既定
# prefix = "Bearer "                      # 既定（大文字小文字は区別しない）
# allow_missing = false                   # true: トークンの無い要求も通す（あれば検証する）
# require_expiration = false
# clock_skew_secs = 60
require_claims = { scope = ["read"] }     # 全部の claim がどれかの値を持つこと
```

- 署名: JWKS の RSA・EC（P-256・P-384）の鍵で RS256/384/512・PS256/384/512・ES256/384。`exp`・`nbf` は時刻のずれを
  見込んで確かめる。
- 文字列の claim は空白で区切る（OAuth の `scope`）。配列はどれかの要素、真偽値・数値は文字列にして照合する。
- JWKS は設定のロード時に読む。鍵の更新はファイルを差し替えて `SIGHUP`。リモートの JWKS の URI は取りに行かない。
- `[route.traffic.cors]` が答える CORS のプリフライトにはトークンが要らない。
- TLS のクライアントの ID による認可は `[tls] client_allowed_sans`（[TLS](tls.md)）。

## セキュリティ強化（systemd サンドボックス化）

io_uringは強力な非同期I/Oインターフェースですが、悪用されるとカーネル権限を奪われるリスクがあります。
このサーバーはsystemdのサンドボックス機能と組み合わせることで、堅牢なセキュリティを実現できます。

### セキュリティアーキテクチャ（多層防御）

```
┌─────────────────────────────────────────────────────────────────┐
│ systemd (PID 1) - 外側の隔離層                                   │
│ ┌─────────────────────────────────────────────────────────────┐ │
│ │ Namespace 隔離 (ProtectSystem, PrivateTmp, PrivateDevices)  │ │
│ │ ┌─────────────────────────────────────────────────────────┐ │ │
│ │ │ veil 内蔵セキュリティ                        │ │ │
│ │ │ ┌─────────────────────────────────────────────────────┐ │ │ │
│ │ │ │ Landlock (ファイルシステム制限)                     │ │ │ │
│ │ │ │ ┌─────────────────────────────────────────────────┐ │ │ │ │
│ │ │ │ │ seccomp (システムコール制限)                    │ │ │ │ │
│ │ │ │ │ ┌─────────────────────────────────────────────┐ │ │ │ │ │
│ │ │ │ │ │ アプリケーション (io_uring + rustls)        │ │ │ │ │ │
│ │ │ │ │ │ - 許可: io_uring_*, socket, read, write...  │ │ │ │ │ │
│ │ │ │ │ │ - 拒否: fork, execve, ptrace, mount...      │ │ │ │ │ │
│ │ │ │ │ └─────────────────────────────────────────────┘ │ │ │ │ │
│ │ │ │ └─────────────────────────────────────────────────┘ │ │ │ │
│ │ │ └─────────────────────────────────────────────────────┘ │ │ │
│ │ └─────────────────────────────────────────────────────────┘ │ │
│ └─────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### 必須システムコール一覧

このサーバーが動作するために必要な最小限のシステムコールです：

| カテゴリ | システムコール | 用途 |
|---------|---------------|------|
| **io_uring** | `io_uring_setup`, `io_uring_enter`, `io_uring_register` | monoio ランタイム |
| **ネットワーク** | `socket`, `bind`, `listen`, `accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `setsockopt`, `getsockopt` | TCP/UDP ソケット |
| **ファイルI/O** | `openat`, `read`, `write`, `close`, `fstat`, `readv`, `writev` | 設定、証明書、ログ |
| **メモリ** | `mmap`, `munmap`, `mprotect`, `brk`, `madvise`, `mremap`, `mlock`, `mlock2` | mimalloc、Huge Pages、io_uring登録バッファ |
| **スレッド** | `clone`, `clone3`, `futex`, `exit_group`, `set_tid_address` | ワーカースレッド |
| **CPUアフィニティ** | `sched_setaffinity`, `sched_getaffinity` | CPUピンニング |
| **シグナル** | `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn` | SIGTERM/SIGHUP |
| **時間** | `clock_gettime`, `nanosleep` | タイムアウト |
| **その他** | `prctl`, `ioctl`, `getrandom`, `fcntl`, `uname` | 各種制御 |

### systemd サービスファイル

`contrib/systemd/veil.service` にサンドボックス化対応のサービスファイルを用意しています。

#### インストール

自動セットアップには `.deb` パッケージ（[packaging/README.md](../../../packaging/README.md)）を使用してください。
以下は手動インストール手順です。

```bash
# 1. 専用ユーザーを作成
sudo useradd -r -s /sbin/nologin veil

# 2. ディレクトリを作成
sudo mkdir -p /var/etc/veil/ssl
sudo mkdir -p /var/log/veil /var/cache/veil /var/tmp/veil
sudo chown -R veil:veil /var/log/veil /var/cache/veil /var/tmp/veil

# 3. 設定ファイルをコピー
sudo cp contrib/config/config.toml /var/etc/veil/config.toml
sudo cp server.crt /var/etc/veil/ssl/cert.pem
sudo cp server.key /var/etc/veil/ssl/key.pem
sudo chown veil:veil /var/etc/veil/ssl/key.pem
sudo chmod 600 /var/etc/veil/ssl/key.pem
sudo chown -R root:veil /var/etc/veil
sudo chmod 0640 /var/etc/veil/config.toml

# 4. バイナリをインストール
sudo cp target/release/veil /usr/bin/veil

# 5. サービスファイルをインストール
sudo cp contrib/systemd/veil.service /etc/systemd/system/
sudo systemctl daemon-reload

# 6. サービスを有効化・起動
sudo systemctl enable veil
sudo systemctl start veil
```

#### 重要な設定項目

```ini
[Service]
# === ユーザー ===
User=veil
Group=veil

# === リソース制限 ===
# io_uring 登録バッファにはメモリロックが必要
LimitMEMLOCK=infinity
LimitNOFILE=1048576

# === ファイルシステム隔離 ===
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
LogsDirectory=veil
CacheDirectory=veil
ReadOnlyPaths=/var/etc/veil

# === 名前空間隔離 ===
RestrictNamespaces=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectKernelTunables=yes

# === ネットワーク ===
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK

# === セキュリティ強化 ===
NoNewPrivileges=yes
# MemoryDenyWriteExecute=yes  # WASM（full ビルド）のため無効
RestrictSUIDSGID=yes

# === システムコール制限 ===
# config.toml の seccomp/Landlock に委譲（SystemCallFilter は使用しない）
SystemCallErrorNumber=EPERM
```

### Huge Pages の有効化

io_uring と mimalloc のパフォーマンスを最大化するには、Huge Pages を有効化します。

```bash
# 1. Huge Pages を確保（128 * 2MB = 256MB）
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# 2. 永続化
echo "vm.nr_hugepages=128" | sudo tee -a /etc/sysctl.d/99-veil.conf
sudo sysctl -p /etc/sysctl.d/99-veil.conf

# 3. systemd で MEMLOCK 制限を解除
# veil.service に LimitMEMLOCK=infinity を設定
```

### セキュリティ検証

サービスのセキュリティ状態を確認する方法：

```bash
# systemd-analyze で設定を検証
systemd-analyze security veil.service

# 実行中のセキュリティ状態を確認
cat /proc/$(pgrep veil)/status | grep -E "Seccomp|NoNewPrivs|CapBnd"

# 期待される出力:
# Seccomp:        2                    # seccomp フィルタが有効
# NoNewPrivs:     1                    # 新規特権取得不可
# CapBnd:         0000000000000c00     # CAP_NET_BIND_SERVICE のみ
```

### トラブルシューティング

#### io_uring が動作しない

```bash
# 原因: システムコールがブロックされている
# 解決: SystemCallFilter に io_uring_* を追加
journalctl -u veil | grep -i "seccomp"

# 手動テスト
sudo strace -f -e trace=io_uring_setup /usr/local/bin/veil -c /etc/veil/config.toml
```

#### メモリロックに失敗

```bash
# 原因: MEMLOCK 制限が低い
# 解決: LimitMEMLOCK=infinity を設定
cat /proc/$(pgrep veil)/limits | grep "locked memory"
```

#### 特権ポート (443/80) にバインドできない

```bash
# 原因: CAP_NET_BIND_SERVICE がない
# 解決 1: systemd で設定
#   AmbientCapabilities=CAP_NET_BIND_SERVICE

# 解決 2: バイナリにケイパビリティを付与
sudo setcap 'cap_net_bind_service=+ep' /usr/local/bin/veil
```

### 代替: bubblewrap との併用

より厳格な隔離が必要な場合は、systemd と bubblewrap を併用できます：

```ini
[Service]
ExecStart=/usr/bin/bwrap \
    --ro-bind /usr /usr \
    --ro-bind /lib /lib \
    --ro-bind /lib64 /lib64 \
    --ro-bind /etc/veil /etc/veil \
    --bind /var/log/veil /var/log/veil \
    --unshare-pid \
    --die-with-parent \
    /usr/local/bin/veil -c /etc/veil/config.toml
```

この構成では、systemd が外側の「器」を作り、bubblewrap がさらに厳格なファイルシステムビューを提供します。

## パニックリカバリー

Veilは接続レベルのパニックキャッチを実装し、高可用性を確保しています。

### 動作

リクエスト処理中にパニックが発生した場合：

| シナリオ | 影響 |
|---------|------|
| **パニックリカバリーなし** | ワーカースレッドがクラッシュし、そのワーカー上の全接続が終了 |
| **Veilのパニックリカバリー** | 影響を受けた接続のみ終了、他の接続は正常に継続 |

### 実装

- `std::panic::catch_unwind` を使用して各接続の非同期タスクをラップ
- パニックはポーリングレベルでキャッチされ、エラーとしてログに記録
- `ConnectionGuard` によりパニック時も接続カウンターが正しくデクリメント
- ワーカースレッドは生存し続け、新しい接続を受け付けを継続

### ログ出力

パニックがキャッチされた場合：
```
[ERROR] Task panicked during poll: Any { .. }
```

### 注意事項

- この機能は自動的に有効化されます（設定不要）
- `monoio::spawn` タスク内のパニックのみを保護
- accept ループやランタイム初期化時のパニックはワーカースレッドを終了させます

## セキュリティ監査（F-236）

リリースの前に、Anthropic の [claude-security](https://github.com/anthropics/claude-plugins-official/tree/main/plugins/claude-security)
プラグインでリポジトリを走査します。確認された指摘は、`docs/backlog/bugs/` の `B-` チケットと同じ変更で直します。v0.8.0 の監査では
B-135〜B-151 を直しました（最初の走査と再走査）。

走査のやり直し方（未コミットのファイルを読ませないよう、監査する版のきれいな worktree で行う）:

```bash
git worktree add ../veil-secscan <revision>
cd ../veil-secscan
CLAUDE_CODE_WORKFLOWS=1 claude -p \
  "/claude-security:claude-security Scan codebase at medium effort. I understand it will use a lot of tokens." \
  --permission-mode auto
```

結果（`CLAUDE-SECURITY-RESULTS.md`・`.jsonl`・`.sarif`）は worktree に書かれます。指摘を直すまでは
`docs/artifacts/claude-security/`（コミットしない）に置きます。前回との比較は `claudeSecurityPluginFindingId` で行い、誤検出と
判断した指摘は理由を `docs/artifacts/claude-security/triage.md` に書きます。

監査から入った要求の検査:

- HTTP/2・HTTP/3 のフィールドの名前は token、値は NUL・CR・LF を含まず空白で始まらず終わらないこと。`:method` は token、
  `:path` は空白・制御文字を含まないこと（RFC 9113 §8.2.1・RFC 9114 §4.2）。違反はストリームのエラーなので、HTTP/1.1 の上流に
  届かない。
- `remove_request_headers`・`add_request_headers` は、すべてのクライアント・上流のプロトコルに効く。
- 応答のキャッシュは、`Set-Cookie` の応答・`key_headers` に無いヘッダで `Vary` する応答・`Authorization` の要求への応答
  （`key_headers` に `Authorization` が無いとき）を保存しない。
- IP の許可・拒否リストは、IPv4 射影の IPv6 のアドレスを IPv4 として照合する。
- 要求のパスは、ルートの照合・`[route.security]` の検査の前に正規化し、正規化したまま上流へ送る
  （`[security] path_normalization`）。絶対形式の要求先は origin 形式に直し、それ以外の origin 形式でないものは 400。
- chunked の要求の本文は厳格に読み（RFC 9112 §7.1。不正なフレーミングは断る）、終端のチャンクより後ろを
  その要求の一部として上流へ送らない。
- WebSocket のアップグレードにも `remove_request_headers`・`add_request_headers` が効く。ハンドシェイクの後ろの
  クライアントのバイトは、上流が `101` を返したときだけ送る。
- HTTP/2・HTTP/3 のバッファ経路の要求本文は、溜める途中で上限（全ルートの `max_request_body_size` の最大。
  最低 10MB）を見て、超えたストリームを断る。
- Linkerd の管理ポートの `/shutdown`・`/env.json` はループバックからだけ受ける。
- メッシュは、ルート・RBAC・JWT の前にパスを正規化し、`remoteIpBlocks` を Envoy の「元のクライアントのアドレス」で判定する。
