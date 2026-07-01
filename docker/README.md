# Veil - High-Performance Reverse Proxy Server

Dockerイメージのビルドとコンテナの実行


ビルド
```sh
# glibc版とmusl版を一度にビルド
./build.sh

# glibc版のビルド
docker build -f Dockerfile.glibc -t "veil:glibc" --build-arg CARGO_FEATURES='full' ..

# musl版をビルド
docker build -f Dockerfile.musl -t "veil:musl" --build-arg CARGO_FEATURES='full' ..
```

dockerを実行する場合io_uringはデフォルトで禁止されている
[seccomp許可リスト](./assets/security/seccomp.json)のjsonを作成してコンテナ実行時に `--security-opt seccomp=<seccomp.json>` で渡し許可する必要がある

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
  --tmpfs /tmp \
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
