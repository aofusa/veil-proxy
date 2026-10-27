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

### 制御レコードと接続の終わり

kTLS ではカーネルが復号するのはアプリケーションデータだけです。それ以外のレコードは veil が扱います。上流からの
TLS 1.3 のセッションチケットは読み捨て、close_notify はストリームの終わり、その他のアラートは接続の中断です。
TLS 1.3 の KeyUpdate は警告を出して接続を閉じます（kTLS は新しい鍵を適用できません。kTLS 無しの rustls は扱えます）。

`Content-Length` も chunked も無い上流の応答は、上流が接続を閉じたところで終わります。TLS（kTLS の有無を問わず）では、
先に close_notify を受けたときだけ完了とみなします。受けずに切れた本文は途中で切れたかもしれないので、キャッシュせず、
バッファリングする応答は 502 にし、クライアントの接続を閉じます。

## TLS セッション再開（F-183）

`[tls] session_resumption` で、HTTP/1.1・HTTP/2 のクライアントのセッション再開の方式を選びます
（HTTP/3 の再開は quiche が扱います）。

| 値 | 動作 |
|---|---|
| `"tickets"`（既定） | ステートレスなセッションチケット。チケット鍵は全ワーカーで共有し、自動でローテーションし、証明書のリロード後も維持します。TLS 1.3 の再開はサーバ側に状態を持たず共有ロックも取らないので、再訪するクライアントがどれだけ多くてもメモリ一定で再開できます。チケットではなく session ID で再開する TLS 1.2 クライアント向けに `session_cache_size` 件のキャッシュも併用します。 |
| `"cache"` | サーバ側のセッションキャッシュのみ（`session_cache_size` 件、全ワーカー共有）。 |
| `"off"` | 再開しない（チケットを送らない）。 |

```toml
[tls]
session_resumption = "tickets"
session_cache_size = 4096   # 件数（"cache"、および "tickets" の TLS 1.2 session ID 用）
tls13_tickets = 1           # TLS 1.3 のハンドシェイクごとに送るチケット数（"off" では 0）
```

F-183 より前は rustls の既定（全ワーカー共有の 256 件のキャッシュと、ハンドシェイクごとに
キャッシュへ入れる TLS 1.3 チケット 2 枚）でした。毎秒数千ハンドシェイクではキャッシュが 1 秒
足らずで入れ替わり、後から戻ってきたクライアントは再開できません。`tools/perf/alloc_ab.sh`
（TLS 1.3、OpenSSL 3 の `wrk`、要求ごとに新しい接続）で、チケット 1,908 req/s、旧既定
1,839 req/s、再開なし 532 req/s でした。

これらのキーの変更には再起動が必要です（証明書のリロードでは起動時の方式を引き継ぎます）。

> 同梱の `williamyeh/wrk` イメージ（LibreSSL 2.7、TLS 1.2 のみ、2018 年）は `"tickets"` では
> 1 接続おきにしか再開しません（再開したハンドシェイクで発行されたチケットを保持しない）。
> そのため `h1tls_hs` は `"cache"` の約 5,500 から約 1,300 req/s に下がります。現行の
> OpenSSL・BoringSSL・NSS・Go・rustls のクライアントは問題なく再開します。こうした古い
> クライアントが TLS 1.3 の無制限な再開より重要なら `"cache"` を使ってください。

## クライアント証明書と上流の mTLS（F-217）

### クライアント証明書を要求する

```toml
[tls]
cert_path = "/etc/veil/cert.pem"
key_path  = "/etc/veil/key.pem"
client_auth = "require"            # "none"（既定） | "optional" | "require"
client_ca   = "/etc/veil/client-ca.pem"
client_allowed_sans = ["spiffe://cluster.local/ns/web/sa/frontend"]   # 任意（F-220）
```

- `"require"` は証明書を出さないクライアントと、`client_ca` で検証できない証明書のクライアントをハンドシェイクで
  拒否する。`"optional"` は証明書を求めるが、出さないクライアントも受ける（出された証明書は検証する）。
- TCP の TLS（HTTP/1.1・HTTP/2）に効く。`[http3]` と併用すると QUIC のクライアントが迂回できるので、起動時にエラーにする。
- `client_allowed_sans` は一覧の SAN（DNS 名・URI・IP）のどれかを持つ証明書だけを受ける。ハンドシェイクで 1 回だけ照合し、
  同じ CA が発行した別の ID は拒否する。`client_auth = "optional"` か `"require"` が要る。
- 証明書のホットリロードは起動時の `client_auth`・`client_ca`・`client_allowed_sans` を引き継ぐ。これらの変更には再起動が要る。

### 上流へクライアント証明書を出す

`[upstreams.<name>]` と `Proxy` の action に書ける:

| キー | 意味 |
|------|------|
| `tls_client_cert` / `tls_client_key` | 上流へ出すクライアント証明書と秘密鍵（PEM、組で指定） |
| `tls_ca` | 上流の証明書の信頼アンカー。`"builtin"`・`"system"`・PEM のパス、またはその配列（[上流の信頼アンカー](#上流の信頼アンカー)）。省略時は `[tls_client] ca` |
| `expected_san` | 上流の証明書の SAN の期待値（DNS 名・URI・IP。どれかが一致すれば受ける）。指定するとホスト名の照合の代わりにこれを使う（SPIFFE ID 等） |

```toml
[upstreams.backend]
servers = ["https://10.0.0.5:8443"]
tls_client_cert = "/etc/veil/upstream-client.pem"
tls_client_key  = "/etc/veil/upstream-client.key"
tls_ca          = "/etc/veil/upstream-ca.pem"
expected_san    = ["spiffe://cluster.local/ns/default/sa/backend"]
```

TLS の設定は設定のロード時に作る（接続ごとには作らない）。プールの接続はこの設定ごとに分けるので、クライアント
証明書の違う上流が接続を共有することはない。証明書のファイルを差し替えたら `SIGHUP` の設定リロードで反映する。
`tls_insecure` より優先する。

### 上流の信頼アンカー

veil が TLS のクライアントになる全経路（HTTPS の上流、ヘルスチェック、VDS・ACME、Proxy-Wasm の
`dispatch_http_call` と gRPC 呼び出し）は、相手の証明書を信頼アンカーで検証する。出どころは次のどれか。

| 値 | 意味 |
|----|------|
| `"builtin"` | veil に組み込んだ CA 一覧（webpki-roots。既定） |
| `"system"` | OS の信頼ストア |
| それ以外 | PEM ファイルのパス。`system`・`builtin` という名前のファイルを指すときは `./system` と書く |

文字列 1 個か、文字列の配列（出どころの和集合）で書く。

```toml
# 外向きの TLS 全体の既定（tls_ca の無い上流、ヘルスチェック、VDS・ACME、Wasm の呼び出し）
[tls_client]
ca = "system"                              # または ["system", "/etc/veil/internal-ca.pem"]

# 上流ごと: [tls_client] ca を置き換える（足し合わせない。足したいときは両方を配列に書く）
[upstreams.billing]
servers = ["https://billing.internal:8443"]
tls_ca = ["system", "/etc/veil/billing-ca.pem"]
```

`[vds] ca_cert` は今までどおり `[tls_client] ca` に足す。`tls_insecure = true` はすべてに優先する。
`[tls_client]` を書かなければ何も変わらない（組み込みの CA を使う）。

OS の信頼ストアの読み先:

| OS | 読み先 |
|----|--------|
| Linux | `SSL_CERT_FILE` / `SSL_CERT_DIR`、無ければ通常のバンドルの場所（`/etc/ssl/certs/ca-certificates.crt`・`/etc/pki/tls/certs/ca-bundle.crt`・`/etc/ssl/cert.pem` など） |
| FreeBSD / OpenBSD / NetBSD | `/etc/ssl/cert.pem`・`/usr/local/share/certs/ca-root-nss.crt`・`/etc/openssl/certs/` |
| macOS | Security.framework（システム・管理者のキーチェーン。信頼設定を反映） |
| Windows | `ROOT` 証明書ストア（ローカルマシンと現在のユーザー） |

- ストアは起動時に、サンドボックス（seccomp・Landlock・capsicum・pledge/unveil・Seatbelt・chroot）を掛ける前に
  1 回だけ読み、プロセスの終わりまで持つ。`landlock_read_paths` に足す必要はない。`SIGHUP` のリロードでは
  読み直さない。OS が CA を更新したら veil を再起動する。
- 起動時に `"system"` を使っていなかった設定へ、リロードで `"system"` を足したとき: サンドボックスが無効なら
  そこで読む。有効ならリロードを拒否してエラーにする（再起動が要る）。黙って組み込みの CA に切り替えることはない。
- 使える証明書が 0 個（ストアが無い場合を含む）は設定のエラー（`veil -t` も失敗する）。解析できない証明書は
  件数つきの警告を出して飛ばす。起動時に読み込んだ件数を `info` で 1 行出す。
- `[tls_client] ca`（や、それが指す PEM）を変えてリロードすると、新しい接続から新しい信頼アンカーを使う。
  古いもので確立したプールの接続は使い回さない。
- パッケージ・アーカイブの既定の設定は `ca = "system"`。コンテナイメージと Helm チャートは `ca = "builtin"`
  （イメージは OS のストアを当てにしない）。既に使っている設定ファイルは書き換えない。
- `tls_ca` にディレクトリ無しで `system`・`builtin` という名前だけを書いていた場合は、キーワードの意味になる。
  ファイルを指すなら `./system` と書く。
- Rust・Python の API ライブラリの既定は `"builtin"`。`tls_client.ca` で選ぶ
  （`App::builder().set("tls_client.ca", "system")`、`overrides={"tls_client.ca": "system"}`）。

## TLS証明書ホットリロード

プロキシを再起動せずにTLS証明書をローテーションします。

### 動作原理

- バックグラウンドスレッドが `reload_interval_secs` 秒ごとに証明書と秘密鍵を確認し、`mtime`・ファイルの実体（シンボリックリンクを辿った先のデバイス + inode）・サイズのいずれかが**変わったら**リロードする（新しくなったときだけではない）。Kubernetes Secret の更新は `..data` シンボリックリンクを新しいディレクトリへ付け替える方式で、差し替え先の `mtime` が同じか古いこともあるため（F-180）。
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

veil は Let's Encrypt の証明書を自分で発行・更新できます（ACME HTTP-01、`--features acme`、`full` に含まれます）。
詳細は [ACME](acme.md) を参照してください。この節の以降は、certbot など外部クライアントを使う代替方法の説明です。

```bash
# 証明書更新（certbot）
certbot renew --deploy-hook "touch /etc/veil/cert.pem"
# veilがmtimeの変化を検知して自動リロード
```

> **注意**: Landlockサンドボックスが有効な場合、証明書ディレクトリを `landlock_read_paths` に含める必要があります。

## コンテナでの証明書・秘密鍵

証明書と秘密鍵はイメージに焼き込まず、環境変数でも渡さず、実行時に**ファイル**としてマウントする（F-180）。
プロセスの環境変数は `/proc/<pid>/environ` から読めるうえメモリからの消去も難しい。ファイルなら権限で守れて、差し替えもできる。

| 基盤 | ファイルの見え方 | 注意 |
|------|----------------|------|
| Docker secrets（`docker compose` / Swarm） | `/run/secrets/<name>` | [`examples/container/compose.secrets.yaml`](../../../examples/container/compose.secrets.yaml) を参照。compose の `file:` は bind mount なので、更新はファイルをその場で上書きする。 |
| Kubernetes Secret ボリューム | `<mountPath>/tls.crt`・`tls.key`（`..data/` へのシンボリックリンク） | kubelet は `..data` リンクを新しいディレクトリへ原子的に付け替えて更新する。veil はこれに追従する（ファイルの実体で比較）。[`deploy/kubernetes/`](../../../deploy/kubernetes/) を参照。`subPath` でマウントすると更新が届かないので使わない。 |

```toml
[tls]
cert_path = "/run/secrets/tls_cert"      # Kubernetes なら /etc/veil/tls/tls.crt
key_path  = "/run/secrets/tls_key"
auto_reload = true
reload_interval_secs = 10

[security]
# ファイルではなく Secret のディレクトリごと許可する（Kubernetes では新しい中身がマウント先の
# 別ディレクトリに置かれるため）。
landlock_read_paths = ["/run/secrets", "/etc/veil", "/var/www"]
```

環境変数 `VEIL_OVERRIDE_<n>` で変えてよいのは証明書の**場所**だけ（例:
`VEIL_OVERRIDE_1='tls.cert_path = "/etc/veil/tls/tls.crt"'`）。PEM の中身は入れない。
`examples/container/e2e.sh` はビルド済みイメージで一連の流れ（Secret での TLS、環境変数の上書き、
証明書の差し替え、graceful stop）を確認する。
