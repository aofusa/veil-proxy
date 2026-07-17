# Veil - High-Performance Reverse Proxy Server

Dockerイメージのビルドとコンテナの実行


ビルド
```sh
# glibc版ビルド
docker build -f Dockerfile.glibc -t "veil:glibc" --build-arg CARGO_FEATURES='full' ..

# musl版ビルド
docker build -f Dockerfile.musl -t "veil:musl" --build-arg CARGO_FEATURES='full' ..
```

io_uringはdockerではデフォルトで禁止されている。そのため [seccomp許可リスト](./assets/security/seccomp.json) を作成しコンテナ実行時に `--security-opt seccomp=<seccomp.json>` で許可する必要がある

> 許可リストには io_uring 系に加え、HTTP/3 データグラムバッチング（F-115）が使う
> `recvmmsg` / `sendmmsg` も含まれる。独自の seccomp プロファイルを使う場合はこれらを
> 許可しないと HTTP/3 の送受信が EPERM で全滅する（defaultAction=ERRNO のため無音で失敗する）
> ので注意。

`--features epoll`（reactor バックエンド、F-120 Phase 2）でビルドしたイメージには io_uring
系 3 syscall を含まない [seccomp-epoll.json](./assets/security/seccomp-epoll.json) を使う
（`--security-opt seccomp=./assets/security/seccomp-epoll.json`）。

テスト実行
```sh
# 自己署名証明書を作成
openssl req -x509 -newkey EC -pkeyopt ec_paramgen_curve:secp384r1 -nodes \
    -keyout "./assets/ssl/key.pem" \
    -out "./assets/ssl/cert.pem" \
    -days 3650 \
    -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
    -addext "basicConstraints=critical,CA:FALSE" \
    -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
    -addext "extendedKeyUsage=serverAuth"

# コンテナを実行
docker run -it --rm \
  -p 80:80 -p 443:443 \
  --read-only \
  --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=512m \
  --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=256m \
  -v ./assets/conf.d:/etc/veil/conf.d:ro \
  -v ./assets/ssl:/etc/veil/ssl:ro \
  -v ./assets/www:/var/www:ro \
  --security-opt seccomp=./assets/security/seccomp.json \
  --name veil-container veil:glibc


# 設定ファイルの正常性確認
docker exec -it veil-container ./veil -t -c /etc/veil/conf.d/config.toml

# 設定ファイルを更新した場合下記で再読み込みさせる
docker kill --signal=SIGHUP veil-container

# 設定変更後など以下でログを確認
docker logs --tail -f veil-container
```
