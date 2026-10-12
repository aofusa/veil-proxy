# ACME: 証明書の自動発行・更新（Let's Encrypt）

[← ドキュメント目次](README.md) · [English](../acme.md)

veil は ACME プロトコル（RFC 8555）の **HTTP-01** チャレンジで、Let's Encrypt などから TLS 証明書を自分で取得・更新できます。
外部ツール（certbot）は不要で、発行した証明書は既存の[証明書ホットリロード](tls.md#tls証明書ホットリロード)で無停止に差し替わります。

> **要件**: `--features acme` でビルド（`--features full` ほか `full*` に含まれます）。

- ACME クライアントは **専用スレッド** で動き、データプレーンには触れません。
- 対応するのは HTTP-01 のみです（DNS-01 は無いので **ワイルドカード証明書は不可**）。ドメイン名がこのサーバを指し、ポート 80 に届く必要があります。

## 設定

```toml
[server]
listen = "0.0.0.0:443"
http2_enabled = true

[tls]
cert_path = "/etc/veil/tls/cert.pem"      # 発行された証明書チェーンをここへ書く
key_path  = "/etc/veil/tls/key.pem"       # 秘密鍵もここへ書く（モード 0600）

[acme]
enabled = true
domains = ["example.com", "www.example.com"]
contact = ["mailto:admin@example.com"]
accept_terms_of_service = true
storage_dir = "/var/lib/veil/acme"
```

| キー | 既定値 | 説明 |
|------|--------|------|
| `enabled` | `false` | ACME を有効にする |
| `directory_url` | `"https://acme-v02.api.letsencrypt.org/directory"` | ACME のディレクトリ URL。試験には Let's Encrypt の **staging** `https://acme-staging-v02.api.letsencrypt.org/directory` を使う |
| `contact` | `[]` | アカウントの連絡先 URL。例: `["mailto:admin@example.com"]` |
| `domains` | `[]` | 証明書に入れるドメイン名（必須で 1 個以上。1 個目が CN で、すべてが SAN になる） |
| `storage_dir` | `"/var/lib/veil/acme"` | アカウント鍵や状態の保存先。モード 0700 で作成され、書き込み可能であること |
| `accept_terms_of_service` | `false` | CA の利用規約に同意する。**`true` が必須**で、そうでないと veil は起動しない |
| `challenge_listen` | `"0.0.0.0:80"` | HTTP-01 応答用の平文リスナーのアドレス。空なら起動しない（後述） |
| `renew_before_days` | `30` | 有効期限がこの日数以内になったら更新する |
| `check_interval_secs` | `43200` | 証明書を確認する間隔（12 時間。最小 60） |
| `directory_ca_cert` | `""` | ACME ディレクトリを検証する追加の CA（PEM）。Pebble などのテスト用サーバ向け |
| `order_timeout_secs` | `300` | 1 回の発行の制限時間（最小 30） |

`[acme]` には `[tls] cert_path` と `key_path` も必要です（証明書の書き込み先であり読み込み元です）。`[acme]` は起動時に読まれ、変更には
再起動が必要です（SIGHUP では反映されません）。他のキーと同様に `VEIL_OVERRIDE_<n>` でも設定できます
（[起動・運用](running.md#環境変数による設定の上書きveil_override_n)を参照）。

## 動作の流れ

1. **起動時（特権降格・サンドボックスの前）。** `[tls] cert_path`/`key_path` が無ければ、`domains` 用の **仮の自己署名証明書** を書き込み、
   TLS リスナーがすぐ起動できるようにして、`storage_dir` に仮証明書であることを示す印を置きます。続いて HTTP-01 の応答リスナーを
   `challenge_listen` で bind します（ポート 80 には root か `CAP_NET_BIND_SERVICE` が必要で、bind は特権を降格する前に行われます）。
   証明書のホットリロードは `[tls] auto_reload` の値にかかわらず自動で有効になります。
2. **発行（ACME スレッド）。** 証明書が仮のもの・読めない・有効期限が `renew_before_days` 以内のいずれかの間、スレッドは ACME アカウント鍵
   （`storage_dir/account.key`、モード 0600。無ければ生成）を用意し、`domains` で注文し、HTTP-01 のトークンを応答リスナーに公開し、CA の検証を待ち、
   新しい証明書の鍵と CSR を作って finalize し、チェーンをダウンロードします。
3. **反映。** 秘密鍵、続いて証明書チェーンを `key_path`・`cert_path` へ **原子的に** 書き込みます（同じディレクトリの一時ファイル → `fsync` → `rename`。
   鍵はモード 0600）。仮証明書の印を消し、すぐに証明書リロードを起こします。以降の新しい TLS ハンドシェイク
   （HTTP/1.1・HTTP/2・HTTP/3）は新しい証明書を使い、既存の接続には影響しません。
4. **更新。** `check_interval_secs` ごとに有効期限を確認し、`renew_before_days` 以内なら上記を繰り返します。失敗したら指数バックオフ
   （5 秒から最大 1 時間）で再試行し、その間も現在の証明書で動き続けます。

### HTTP-01 応答リスナー

応答リスナーは専用スレッドで動く小さな平文 HTTP/1.1 サーバです。`GET /.well-known/acme-challenge/<token>` には key authorization を返し、
それ以外のリクエストには `https://<Host><path>` への `301` リダイレクトを返すので、ポート 80 を HTTP→HTTPS リダイレクトとしても使えます
（そのために `[server] http` を使う必要はありません。同じポートに両方を bind しないでください）。同時に処理する接続は最大 64 です。

すでに別のもの（ロードバランサ・別のプロキシ・Kubernetes の Ingress）がポート 80 を受けているなら、`challenge_listen = ""` にするか
`"0.0.0.0:8080"` のような別ポートにして、`/.well-known/acme-challenge/` をそこへ転送してください。

## 要件と運用

- `domains` の名前がこのサーバを指し、**インターネットからポート 80（TCP）に到達できる** こと（CA は `challenge_listen` に関係なく各名前のポート 80 に接続します）。
- `cert_path`・`key_path` のディレクトリと `storage_dir` が、特権降格後の実行ユーザで書き込めること。`storage_dir` は永続化してください
  （コンテナならボリューム）。さもないと起動のたびに新しいアカウントが作られ、レート制限に掛かります。
- **Landlock** サンドボックスが有効なら、証明書のディレクトリと `storage_dir` を `landlock_write_paths` に（証明書のディレクトリは
  `landlock_read_paths` にも）含めてください。seccomp は通常のネットワーク系 syscall を許可している必要があります（既定で許可されています）。
  コンテナについては[コンテナでの証明書・秘密鍵](tls.md#コンテナでの証明書秘密鍵)を参照してください。
- 設定中は staging ディレクトリ（`https://acme-staging-v02.api.letsencrypt.org/directory`）を使ってください。本番の CA は厳しいレート制限を設けています。
  staging の証明書はブラウザに信頼されません。veil は証明書が仮のもの・期限間近のときしか再発行しないので、`directory_url` を本番へ切り替えるときは、
  veil を止めて staging の `cert_path`・`key_path` のファイルを削除してから起動し直してください（仮証明書が書かれ、本番の証明書が発行されます）。
- 同じアカウントを使い続けたいなら `storage_dir/account.key` をバックアップしてください。
- ログは `[ACME]` で始まります: `wrote a temporary self-signed certificate`・`managing a certificate for ...`・`certificate installed`・
  `issuance failed (retry in ...)`。

## Pebble でのテスト

[Pebble](https://github.com/letsencrypt/pebble) は Let's Encrypt の ACME テスト用サーバです。リポジトリには、Docker で Pebble を起動し、veil が
HTTP-01 で証明書を取得して、再起動なしに仮証明書から発行済みの証明書へ切り替わることを確かめる E2E テストがあります。

```bash
cargo build --features full
tests/acme/pebble_e2e.sh [path/to/veil]      # docker・openssl・curl が必要
```

`directory_url = "https://localhost:14000/dir"`、`directory_ca_cert`（Pebble の minica）、`challenge_listen = "[::]:5002"` を使い、
仮証明書・発行・HTTPS で配信される内容・アカウント鍵と仮証明書の印の状態・鍵ファイルのモード（0600）を検証します。

## 代替: certbot

veil 内蔵の ACME は任意です。外部のクライアントを使い続け、veil にファイルを拾わせることもできます。

```toml
[tls]
cert_path = "/etc/letsencrypt/live/example.com/fullchain.pem"
key_path  = "/etc/letsencrypt/live/example.com/privkey.pem"
auto_reload = true
```

```bash
certbot renew --deploy-hook "touch /etc/letsencrypt/live/example.com/fullchain.pem"
```

[TLS証明書ホットリロード](tls.md#tls証明書ホットリロード)を参照してください。1 つの証明書にはどちらか一方の方式だけを使い、
certbot が管理する証明書に `[acme]` を有効にしないでください。
