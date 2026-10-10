# 起動と運用

[← ドキュメント目次](README.md) · [English](../running.md)

## 起動

```bash
# デフォルト設定ファイル（/etc/veil/config.toml）で起動
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
| `-c, --config <PATH>` | 設定ファイルのパス | `/etc/veil/config.toml` |
| `-t, --test` | 設定ファイルの構文と内容を検証して終了（nginx -t 相当） | - |
| `-o, --override <KEY=VALUE>` | config.toml の値をコマンドラインから上書き（繰り返し指定可、後述） | - |
| `-h, --help` | ヘルプメッセージを表示 | - |
| `-V, --version` | バージョン情報を表示 | - |

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
2. 既存のリクエスト処理を完了
3. 全ワーカースレッドの終了を待機
4. プロセス終了

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

> **FreeBSD capsicum の capability mode は fail-closed**（F-181）: `capsicum_capability_mode = true` を指定したのに capability mode に入れない場合、veil は理由をログに出して **終了コード 1 で終了します**。入れないのは、起動後に `connect(2)`/`bind(2)` が要る構成（`Proxy` ルート・`[upstreams]`・`[[l4]]`・h2c・HTTP/3・HTTP リダイレクトリスナー）、`cap_enter(2)` の失敗、ワーカーのリスナー bind が終わらない場合です。弱い rights 制限のサンドボックスへ黙って切り替えることはしません。他のサンドボックスと同じく、`[security] allow_security_failures = true` のときだけ警告して rights 制限のみで続行します。

> **FreeBSD capsicum の capability mode**（`capsicum_capability_mode = true`）: SIGHUP（と admin API のリロード）で設定ファイルを読み直せます。`cap_enter` の前に設定ファイルのあるディレクトリ（とアクセスログのディレクトリ）を開いておき、リロードのたびにファイル名を `openat(2)` + `O_RESOLVE_BENEATH` で開き直します。rename による置き換えや、そのディレクトリ内の相対シンボリックリンクの差し替え（Kubernetes の ConfigMap 方式）にも追従します（F-178）。TLS 証明書も `[tls] auto_reload = true` なら同じ仕組みでリロードされます（F-136）。
>
> capability mode のプロセスは新しいディレクトリを開くことも、接続・bind することもできません。これらが必要になるリロードは **拒否して以前の設定を維持します**（ログに `capability mode: ...; restart veil to apply this change`）。次の変更は再起動で反映してください。
>
> - 起動時に `File` ルートだったディレクトリの配下にないパスを指す `File` ルート（そのディレクトリの外にある単一ファイルのルートを含む）
> - `Proxy` ルート・`[upstreams]`・`[[l4]]`・h2c・HTTP/3・HTTP リダイレクトリスナー（起動時に capability mode へ入るかどうかも同じ規則で決まります）
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
