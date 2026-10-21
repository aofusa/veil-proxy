# コンテナイメージ

veil はマルチアーキ（amd64・arm64）のコンテナイメージとして **`ghcr.io/aofusa/veil`** に公開しています。

| タグ | 中身 |
|------|------|
| `latest`・`musl`・`musl-vX.Y.Z` | musl の静的バイナリ（`scratch`） |
| `glibc`・`glibc-vX.Y.Z` | glibc のバイナリ（distroless） |

どちらも Docker の既定の seccomp と Kubernetes の `RuntimeDefault` の下で動きます。中のバイナリは `full-container` のビルド
（epoll の readiness reactor。[ビルド](building.md)）です。版のタグは上書きしません。

## すぐ試す

```sh
docker run --rm -p 8080:8080 ghcr.io/aofusa/veil
```

<http://localhost:8080/> を開きます。

- イメージに内蔵の設定は、`/var/www` の内蔵のページを 8080 番で配ります（平文の HTTP/1.1 と h2c）。TLS の証明書は要りません。
- 要求ごとのアクセスログを標準出力に出します（`docker logs` で見られます）。
- ページから `/quickstart.html`（この文書と同じ手順。英語）へ移れます。

```sh
curl -i http://localhost:8080/
curl --http2-prior-knowledge -I http://localhost:8080/
```

## Quickstart: 自分のファイルと設定を使う

| イメージの中のパス | 内容 | 差し替え方 |
|-------------------|------|-----------|
| `/var/www/` | 内蔵のページ | `-v "$PWD/site:/var/www:ro"` |
| `/etc/veil/conf.d/config.toml` | 内蔵の設定 | `-v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro"` |
| `/etc/veil/ssl/` | （イメージには無い）TLS の証明書と秘密鍵 | `-v "$PWD/ssl:/etc/veil/ssl:ro"` |

### 自分の HTML を配る

```sh
mkdir -p site && echo '<h1>Hello from veil</h1>' > site/index.html
docker run --rm -p 8080:8080 -v "$PWD/site:/var/www:ro" ghcr.io/aofusa/veil
```

### 自分の config.toml を使う

内蔵の設定（[`packaging/container/config.toml`](../../../packaging/container/config.toml)）を元に、要るところだけ変えます。
全キーの説明は [設定リファレンス](configuration.md) と [`examples/config.toml`](../../../examples/config.toml) にあります。

```sh
docker run --rm -p 8080:8080 \
  -v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro" \
  -v "$PWD/site:/var/www:ro" \
  ghcr.io/aofusa/veil

# 起動せずに設定を検証する
docker run --rm -v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro" \
  ghcr.io/aofusa/veil /veil -t -c /etc/veil/conf.d/config.toml
```

別のコンテナへのリバースプロキシの例です。ルートは書いた順に評価され、最初に一致したものが使われます
（[ルーティング](routing.md)）。そのため、`/api/` は `/` より前に書きます。

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[[route]]
[route.conditions]
path = "/api/"
[route.action]
type = "Proxy"
url = "http://backend:3000"

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
```

### TLS と HTTP/3

```toml
[server]
listen = "0.0.0.0:443"              # TLS: HTTP/1.1 と HTTP/2（ALPN）
listen_cleartext = "0.0.0.0:8080"   # enable_secure_redirect = true（既定）なら https への 301 を返す

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"

[http3]
enabled = true                      # [server].listen と同じアドレスの UDP
```

```sh
docker run --rm -p 8080:8080 -p 443:443/tcp -p 443:443/udp \
  -v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro" \
  -v "$PWD/ssl:/etc/veil/ssl:ro" -v "$PWD/site:/var/www:ro" \
  ghcr.io/aofusa/veil
```

証明書の再読み込みと ACME は [TLS](tls.md)、HTTP/3 は [プロトコル](protocols.md) を参照してください。

## コンテナの運用

- **リロード**: `docker kill -s HUP <container>` で、接続を切らずに設定と証明書を読み直します（[起動と運用](running.md)）。
- **停止**: `docker stop`（SIGTERM）で正常に止まります。
- **権限**:
  - veil は root で起動し、リスナーを bind してから `nonroot`（uid/gid 65532）に権限を落とします。自分の seccomp と Landlock も掛けます。
  - マウントするファイルは uid 65532 が読めるようにします。
  - 内蔵の Landlock の一覧に無いディレクトリは、`[security] landlock_read_paths` に足します（[セキュリティ](security.md)）。
- **読み取り専用のルート**: 動きます。キャッシュやリクエストのバッファリングを使うときは、`/var/cache/veil` と `/var/tmp/veil` に `tmpfs` を与えます。
- **ログ**: アプリケーションのログとアクセスログは標準出力、エラーは標準エラーに出ます（[可観測性](observability.md)）。

## Kubernetes

設定は ConfigMap から `/etc/veil/conf.d/config.toml` に、証明書は Secret から `/etc/veil/ssl` にマウントします。
マニフェスト・サイドカー・サービスメッシュのデータプレーンは [Kubernetes](kubernetes.md) を参照してください。

## 自分でイメージをビルドする

```sh
docker build -f docker/Dockerfile.musl --build-arg CARGO_FEATURES=full-container -t veil:musl .
tools/release/image-smoke.sh veil:musl defaults
```

Dockerfile とビルドの引数の一覧（epoll への切り替え付きの io_uring 版を含む）は [`docker/README.md`](../../../docker/README.md) にあります。
