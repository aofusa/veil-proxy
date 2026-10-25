# 起動と運用

[← ドキュメント目次](README.md) · [English](../running.md)

## 起動

```bash
# 既定のパスの設定ファイルで起動（下の「設定ファイル」）
./veil

# 設定ファイルを指定して起動
./veil -c /path/to/config.toml
./veil --config /path/to/config.toml

# ヘルプを表示
./veil --help

# バージョンを表示
./veil --version
```

### コマンドラインオプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `-c, --config <PATH>` | 設定ファイルのパス | `$VEIL_CONFIG`、無ければ既定のパス（下記） |
| `-t, --test` | 設定ファイルの構文と内容を検証して終了（nginx -t 相当） | - |
| `-o, --override <KEY=VALUE>` | config.toml の値をコマンドラインから上書き（繰り返し指定可、後述） | - |
| `-h, --help` | ヘルプメッセージを表示 | - |
| `-V, --version` | バージョン情報を表示 | - |

### 設定ファイル

veil は常に設定ファイルを読みます。設定は内蔵していません。読むファイルは次の順で決まります。

1. `-c FILE` / `--config FILE`
2. 環境変数 `VEIL_CONFIG`（空なら無視）
3. 既定のパス

   | ビルド | 既定のパス |
   |---|---|
   | Linux・macOS・FreeBSD・OpenBSD・NetBSD | `/etc/veil/config.toml` |
   | Windows | `%ProgramData%\veil\config.toml` |
   | ビルド時に `VEIL_DEFAULT_CONFIG_PATH=/絶対パス` を設定した場合（BSD の ports） | その値 |

   実行するバイナリの既定のパスは `veil --help` に出ます。

ファイルが無いときは、探したパスを示して終了コード 1 で止まります。起動ログには、ファイルとそのパスの決まり方が出ます
（`Config File: /etc/veil/config.toml (from default path)`）。SIGHUP のリロードも同じファイルを読み直します。

#### パッケージが既定の設定を置く場所

各パッケージは既定の設定と既定のページを置き、veil はオプション無しでそれを読みます。既定の設定は、そのページを
`127.0.0.1:8080`（コンテナは `0.0.0.0:8080`）の平文の HTTP/1.1・h2c で配り、要求ごとにアクセスログを出します。
ページ（`/quickstart.html`）にもこの表があります。

| 入れ方 | 設定 | ページ | veil がファイルを見つける方法 | 起動 |
|---|---|---|---|---|
| コンテナイメージ | `/etc/veil/conf.d/config.toml` | `/var/www/` | イメージの `CMD` が `-c` を渡す | `docker run -p 8080:8080 ghcr.io/aofusa/veil` |
| deb・rpm | `/etc/veil/config.toml` | `/usr/share/veil/www/` | 既定のパス | Debian・Ubuntu は入れると起動、RPM は `sudo systemctl enable --now veil` |
| Homebrew | `$(brew --prefix)/etc/veil/config.toml` | `$(brew --prefix)/share/veil/www/` | `veil` の wrapper が `VEIL_CONFIG` を設定 | `brew services start veil` か `veil` |
| winget | `%ProgramData%\veil\config.toml` | `%ProgramData%\veil\www\` | 既定のパス | `veil` |
| FreeBSD ports | `/usr/local/etc/veil/config.toml` | `/usr/local/share/veil/www/` | ビルド時に入れた既定のパス | `sysrc veil_enable=YES && service veil start` |
| OpenBSD ports | `/etc/veil/config.toml` | `/usr/local/share/veil/www/` | 既定のパス | `rcctl enable veil && rcctl start veil` |
| NetBSD pkgsrc | `/usr/pkg/etc/veil/config.toml` | `/usr/pkg/share/veil/www/` | ビルド時に入れた既定のパス | `/etc/rc.conf` に `veil=YES`、`service veil start` |

パッケージを上げても、書き換えた設定は上書きされません。

#### 入れ方

バイナリだけのもの以外は、上の既定の設定とページも一緒に入ります。

##### apt・dnf のリポジトリ

Debian・Ubuntu（amd64・arm64）と、RHEL 互換・Fedora・Amazon Linux（x86_64・aarch64）向けの署名付きのリポジトリです。
署名の鍵の指紋は `42FF D5A9 145A BEAD 9CDE  F997 AF5B 42F9 5AC9 AEB9` です。

```bash
# Debian 12・13、Ubuntu 22.04・24.04
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://aofusa.github.io/veil-packages/veil.gpg | sudo tee /etc/apt/keyrings/veil.gpg >/dev/null
echo "deb [signed-by=/etc/apt/keyrings/veil.gpg] https://aofusa.github.io/veil-packages/apt stable main" \
  | sudo tee /etc/apt/sources.list.d/veil.list
sudo apt update && sudo apt install veil      # サービスが起動する
curl http://127.0.0.1:8080/

# RHEL 9・10 と互換、Fedora、Amazon Linux 2023
sudo curl -fsSL https://aofusa.github.io/veil-packages/veil.repo -o /etc/yum.repos.d/veil.repo
sudo dnf install veil
sudo systemctl enable --now veil
curl http://127.0.0.1:8080/
```

`.deb`・`.rpm` は GitHub のリリースにも付いています。ログは journal に出ます（`journalctl -u veil`）。設定を変えたら
`sudo systemctl reload veil` で読み直します。`apt remove` は `/etc/veil/config.toml` を残し、`apt purge` は消します。

0.8.x（設定を `/var/etc/veil/config.toml` に置いていた）から上げる場合: パッケージはそのファイルを使い続けます
（`/etc/default/veil` か `/etc/sysconfig/veil` に `VEIL_CONFIG=/var/etc/veil/config.toml` を書く）。既定のパスへ移すときは、
設定を `/etc/veil/config.toml` に写し、その行を消して `sudo systemctl restart veil` を実行してください。0.8.x が
`/var/etc/veil/ssl/` に作った自己署名の証明書は、そのまま残ります。

##### Homebrew（macOS・Linux）

```bash
brew install aofusa/tap/veil
brew services start veil        # または端末で veil を起動
curl http://127.0.0.1:8080/
```

`veil` コマンドは wrapper で、自分で `VEIL_CONFIG` を設定していなければ `$(brew --prefix)/etc/veil/config.toml` を
設定します。`veil -t` と `veil` はそのファイルを使います。設定を変えたら `brew services restart veil` で反映します。

##### winget（Windows）

```powershell
winget install Aofusa.Veil      # 管理者で。全利用者向けに入る
veil                            # 新しい端末で。http://127.0.0.1:8080/ を開く
```

MSI は `C:\Program Files\veil\veil.exe`（`PATH` に足す）、設定 `%ProgramData%\veil\config.toml`、ページ
`%ProgramData%\veil\www\` を入れます。`%ProgramData%\veil` のファイルを変えられるのは管理者だけです。上げても
`winget uninstall` しても、設定は残ります。Windows のサービスには登録しません。サービスとして動かすときは
`sc.exe create veil binPath= "C:\Program Files\veil\veil.exe"` か、サービスの wrapper を使ってください。

##### Rust（`cargo binstall`・`cargo install`）

```bash
cargo binstall veil-proxy                                   # GitHub のリリースのビルド済みのバイナリ
cargo install veil-proxy --locked --features full           # crates.io からビルド（Linux）
```

どちらもバイナリだけを入れます（下記）。`cargo binstall` は各アーカイブの minisign の署名を確かめます。
`cargo install` の feature は OS に合わせて選んでください: `full`（Linux）・`full-macos`・`full-windows`・
`full-freebsd`・`full-openbsd`・`full-netbsd`。`--features` を付けないと既定の feature（kTLS・HTTP/2）だけになります。
ビルドには C コンパイラと `cmake`、Windows では `nasm` が要ります（[ビルド](building.md)）。

#### バイナリだけを使う場合は設定を自分で用意する

リリースのアーカイブ・`cargo install veil-proxy`・`cargo binstall veil-proxy` で入るのは `veil` のバイナリだけです。
設定ファイルとページは入りません（cargo にはデータのファイルを入れる仕組みが無く、veil も自分では書きません）。
自分で用意してください。既定の設定とページが入った状態で使いたいときは、パッケージかコンテナイメージを使ってください。

リリースのアーカイブの `examples/` に、出発点になる例があります。

```bash
cd veil-<version>-<target>
./veil -c examples/config.toml        # examples/www/ を http://127.0.0.1:8080/ で配る
```

最初から書く場合の最小の設定（`/srv/www` に `index.html` を置く）:

```toml
[server]
listen_cleartext = "127.0.0.1:8080"   # 平文の HTTP/1.1 + h2c
enable_secure_redirect = false        # TLS のリスナーが無いので https へリダイレクトしない

[access_log]
enabled = true

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/srv/www/"
```

`veil -c /path/to/config.toml` で起動するか、`VEIL_CONFIG=/path/to/config.toml` を設定するか、既定のパスに置いてください。
設定の中の相対パスは、veil を起動したディレクトリからの相対です。

### 設定の上書き（`-o`/`--override`）

`config.toml` の任意のキーを、ファイルを編集せずコマンドラインから上書きできます。
繰り返し指定可能（`-o` 1 個につき 1 個のオーバーライド）で、起動時・`-t` 検証時・
**ホットリロード（SIGHUP）時のすべて** で同じグローバルなオーバーライド集合が適用される
（設定ファイルを再パースするたびに同じ経路を通るため）。

構文: `<path> = <toml-value>`（`=` 前後の空白は任意）。

- `<path>` はドット区切りのキーパス（例: `server.threads`、`tls.cert_path`、
  `http3.mmsg_batch_size`）。各セグメントは次のいずれか:
  - 裸のキー: `[A-Za-z0-9_-]+`
  - クオート文字列（`"..."` または `'...'`）: キー自体にドットを含む場合に使う
  - 10 進数の配列インデックス（親が配列である場合のみ有効。例: `l4.0.listen`）
  - 利便性のため、任意のセグメントを TOML のセクション記法風に `[ ]` で囲んでよい
    （`[server].threads = 1` は `server.threads = 1` と等価。セグメント解釈前に
    先頭の `[` と末尾の `]` を 1 個だけ取り除く）。
- `<toml-value>` は TOML 値としてそのままパースする（`1`、`"str"`、`true`、`1.5`、
  `[1, 2]`、`{ a = 1 }` などが使える）。クオート無しの裸の値が TOML 値として
  パースできず、かつ `"`・`'`・`[`・`]`・`{`・`}` のいずれも含まない場合に限り、
  1 回だけ文字列リテラルとして再解釈する（これにより
  `-o "tls.cert_path = /etc/veil/cert.pem"` のようにパスをクオート無しで書ける）。
  これらの記号を含みながら TOML として不正な値はハードエラー（文字列への
  暗黙フォールバックはしない）。

```bash
# スカラー値の上書き
./veil -o "server.threads = 4"

# ブラケット記法（[section].key）。上と等価
./veil -o "[server].threads = 4"

# クオート無しの文字列値
./veil -o "tls.cert_path = /etc/veil/cert.pem"

# 複数指定（配列インデックスを含む）
./veil -o "server.threads = 4" -o "l4.0.listen = 0.0.0.0:9000"
```

### 環境変数による設定の上書き（`VEIL_OVERRIDE_<n>`）

同じ上書きを環境変数でも指定できます（コンテナや systemd 向け、F-179）。1 個の変数に 1 個の上書きを
`--override` と同じ書式で入れ、名前は `VEIL_OVERRIDE_<n>`（`<n>` は 10 進の正整数）にします。

```bash
VEIL_OVERRIDE_1='server.listen = "0.0.0.0:8443"' \
VEIL_OVERRIDE_2='route.0.action.path = "/srv/www"' \
./veil -c /etc/veil/config.toml
```

| 規則 | 挙動 |
|------|------|
| 名前 | `VEIL_OVERRIDE_<n>`。`<n>` は 10 進の正整数（`VEIL_OVERRIDE_1`、`VEIL_OVERRIDE_20`）。それ以外（`VEIL_OVERRIDE_`・`VEIL_OVERRIDE_X`・`VEIL_OVERRIDE_0`）は起動エラー。 |
| 適用順 | `<n>` の数値の昇順（`2` が `10` より先）。同じパスを複数回指定したら後勝ち。 |
| 優先順位 | `config.toml` < 環境変数 < コマンドラインの `-o`/`--override`。 |
| リロード | 環境変数はプロセス起動時の値で固定。SIGHUP で読み直すのは `config.toml` だけで、同じ環境変数の上書きを当て直す。 |
| 機密値 | 秘密鍵やトークンをこの方法で渡さない（プロセスの環境変数は Linux の `/proc/<pid>/environ` や BSD の `ps -e` で見える）。ファイルとしてマウントする（[コンテナでの証明書・秘密鍵](tls.md#コンテナでの証明書秘密鍵)）。 |

### 設定ファイルの検証

デプロイやリロード前に設定ファイルを検証できます：

```bash
# デフォルト設定ファイルをテスト
./veil -t

# 指定設定ファイルをテスト
./veil -t -c /path/to/config.toml
```

**検証内容:**
- TOML構文のパース
- 設定値のバリデーション
- TLS証明書・秘密鍵ファイルの存在確認

**出力例:**
```bash
# 成功
veil: configuration file config.toml test is successful

# 失敗（TLS証明書が見つからない）
veil: configuration file config.toml test failed
veil: TLS certificate not found: /path/to/cert.pem
```

**注意**: SIGHUPによる設定リロード時、新しい設定が不正な場合はリロードが拒否され、サーバーは以前の有効な設定で動作を継続します。

### リロードで変えられない設定

SIGHUP のリロードはルート・アップストリーム・セキュリティなど大半の設定を反映しますが、待ち受けは起動時に確定します。
次の項目をファイルで変えてリロードしても **反映されません**。veil は項目名を列挙した警告を 1 回出し
（`Listener settings changed (...); restart veil to apply them`）、古い待ち受けのまま動き続けます。

- `[server] listen`・`listen_cleartext`・`threads`
- `[server] http2_enabled`（TLS の ALPN は起動時に作る。h2 を選んだ接続は HTTP/2 のまま処理される）
- `[http3] enabled`・`listen`
- `[[l4]]` のリスナー（`name`・`listen`・`protocol`）
- `[[listener]]`（追加・削除、`name`・`listen`・`tls`・`http2`・`http3`・`http3_listen`・`threads`・`unix_socket_permissions`）。
  平文の方針（`allow_http`・`allow_h2c`・`secure_redirect`・`redirect_to`）は `listen_cleartext` の `[server]` のキーと同じく
  リロードで **反映される**。ただしリスナーの列そのものが変わったリロードでは、再起動まで起動時の方針のまま
- TLS 証明書は設定のリロードではなく、別の証明書ホットリロードで更新される

## 設定ファイルバリデーション

起動時に設定ファイルの詳細な検証を行い、問題があれば明確なエラーメッセージを出力します。

### 検証項目

| 項目 | チェック内容 |
|------|-------------|
| TLS証明書 | ファイルの存在確認 |
| TLS秘密鍵 | ファイルの存在確認 |
| リッスンアドレス | 有効なソケットアドレス形式 |
| Upstream URL | 有効なURL形式 |
| プロキシURL | 有効なURL形式 |
| ファイルパス | ファイル/ディレクトリの存在確認 |
| ファイルモード | `sendfile` または `memory` |

### エラーメッセージ例

```
Error: TLS certificate file not found: /path/to/cert.pem
Error: Invalid proxy URL for route 'example.com:/api/': invalid-url
Error: Upstream 'backend-pool' not found
```

## Graceful Shutdown

SIGINT（Ctrl+C）またはSIGTERMを受信すると、サーバーは安全に終了します：

1. 新規接続の受付を停止
2. アイドルなキープアライブ接続をすぐ閉じる（次の要求を待っている HTTP/1.1 接続。ストリームの無い
   HTTP/2 接続には `GOAWAY(NO_ERROR)` を送る。HTTP/3 接続には `GOAWAY` を送り、処理中の要求が終わったら
   `H3_NO_ERROR` で閉じる）
3. 処理中のリクエストを完了（各接続は最後の応答を返してから閉じる。HTTP/2・HTTP/3 は GOAWAY の後に
   開かれたストリームを処理しない）
4. 全ワーカースレッドの終了を待機
5. プロセス終了

通常の運用中も、ストリームの無い HTTP/2 接続は 60 秒無通信だと `GOAWAY(NO_ERROR)` を送って閉じます
（HTTP/1.1 のキープアライブと同じアイドルタイムアウト）。

既存接続を待つ時間の上限は `[server] graceful_shutdown_timeout_secs`（既定 30 秒）です。過ぎても残っている
接続は閉じ、プロセスは正常終了（終了コード 0）します。unix では両シグナルを専用のシグナルスレッドで受けるため、
コンテナの `docker stop`（SIGTERM）でも SIGKILL を待たずに graceful shutdown します。

```bash
# サーバー起動
./veil -c ./examples/config.toml &

# 安全に終了する
kill -SIGTERM $!
# または Ctrl+C
```

## Graceful Reload（ホットリロード）

SIGHUPを受信すると、サーバーは設定ファイルを再読み込みします。
既存の接続は中断されず、新しい接続から新しい設定が適用されます。

### 動作

1. SIGHUPシグナルを受信
2. 起動時に指定した設定ファイルを再読み込み
3. 設定のバリデーション
4. `ArcSwap` によるロックフリーな設定更新
5. 新規接続は新しい設定を使用

> **Note**: リロード時は起動時に `-c` オプションで指定したパス（またはデフォルトの `/etc/veil/config.toml`）が使用されます。

> **FreeBSD capsicum の capability mode は fail-closed**（F-181）: `capsicum_capability_mode = true` を指定したのに capability mode に入れない場合、veil は理由をログに出して **終了コード 1 で終了します**。入れないのは、起動後に `bind(2)` が要る構成（`[[l4]]`・平文リスナー（`listen_cleartext`）・HTTP/3）、接続ブローカーを起動できない場合、`cap_enter(2)` の失敗、ワーカーのリスナー bind が終わらない場合です。弱い rights 制限のサンドボックスへ黙って切り替えることはしません。他のサンドボックスと同じく、`[security] allow_security_failures = true` のときだけ警告して rights 制限のみで続行します。

> **FreeBSD capsicum の capability mode でのプロキシ（接続ブローカー）**（F-182）: capability mode のままでも `Proxy` ルートと `[upstreams]` を使えます。`cap_enter` の前（特権降格の後）に、veil はサンドボックスの外で動く **接続ブローカー** を起動します。これは veil 自身を再実行した子プロセスで、本体と同じ利用者で動きます。ブローカーが受け付けるのは **起動時に確定した許可リスト** の添字だけです。許可リストは `[upstreams]` の全サーバ、`Proxy` ルートの URL、OpenTelemetry のエンドポイントです。ブローカーは非ブロッキングの `connect(2)` を始め、そのソケットを `SCM_RIGHTS` で本体へ渡します。TLS と HTTP はサンドボックス内の本体が処理します。そのため、ワーカーが乗っ取られても、ブローカーを使って設定された上流以外へは接続できません。ホスト名はブローカーが解決します（30 秒ごとに再解決）。追加のコストがかかるのは新しい上流接続を開くときだけです（上流の接続はプール・多重化で再利用します）。ブローカー自身も防御を固めています（ptrace とコアダンプの禁止、fork 不可、ファイル書き込み不可、veil と一緒に終了）。ブローカーが終了した場合、veil は終了コード 1 で終わり、サービスマネージャによる両方の再起動に任せます。

> **FreeBSD capsicum の capability mode**（`capsicum_capability_mode = true`）: SIGHUP（と admin API のリロード）で設定ファイルを読み直せます。`cap_enter` の前に設定ファイルのあるディレクトリ（とアクセスログのディレクトリ）を開いておき、リロードのたびにファイル名を `openat(2)` + `O_RESOLVE_BENEATH` で開き直します。rename による置き換えや、そのディレクトリ内の相対シンボリックリンクの差し替え（Kubernetes の ConfigMap 方式）にも追従します（F-178）。TLS 証明書も `[tls] auto_reload = true` なら同じ仕組みでリロードされます（F-136）。
>
> capability mode のプロセスは新しいディレクトリを開くことも、接続・bind することもできません。これらが必要になるリロードは **拒否して以前の設定を維持します**（ログに `capability mode: ...; restart veil to apply this change`）。次の変更は再起動で反映してください。
>
> - 起動時に `File` ルートだったディレクトリの配下にないパスを指す `File` ルート（そのディレクトリの外にある単一ファイルのルートを含む）
> - 起動時に確定した接続ブローカーの許可リストに無い上流（新しい `Proxy` ルートの URL や `[upstreams]` のサーバ。同じ上流のままルート・重み・ヘルスチェックを変えるのは問題ありません）
> - `[[l4]]`・平文リスナー（`listen_cleartext`）・HTTP/3（起動時に capability mode へ入るかどうかも同じ規則で決まります）
> - アクセスログの有効化と `[access_log] file_path` の変更
>
> 同じパスのアクセスログは開き直せるので、logrotate の「移動してから SIGHUP」によるローテーションに対応します。設定ファイルのディレクトリはプロセスから読める状態で残るため、設定は専用ディレクトリに置いてください。

```bash
# 設定ファイルを編集
vim examples/config.toml

# 設定を再読み込み（ゼロダウンタイム）
kill -SIGHUP $(pgrep veil)
```

### 対応する変更

| 項目 | ホットリロード対応 |
|------|-------------------|
| ルーティング設定 | ✅ |
| セキュリティ設定 | ✅ |
| Upstream設定 | ✅ |
| TLS証明書（HTTP/1.1・HTTP/2・HTTP/3） | ✅（`[tls] auto_reload = true` 時。詳細は「TLS証明書ホットリロード」節） |
| リッスンアドレス | ❌（再起動が必要） |
| ワーカースレッド数 | ❌（再起動が必要） |
