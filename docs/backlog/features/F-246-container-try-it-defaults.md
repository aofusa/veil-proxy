# F-246: 公開イメージを `docker run` だけで試せるようにし、コンテナの使い方の文書を足す

**状態: 実装中（feat/v083-release、2026-10-21。v0.8.3 で公開）**

## 目的

- `docker run --rm -p 8080:8080 ghcr.io/aofusa/veil` だけで veil が起動し、`http://localhost:8080/` で内蔵の
  `index.html` が返るようにする。利用者は何も用意しなくてよい（API ライブラリの既定の設定と同じ考え方。F-244）。
- ghcr.io のパッケージのページに、すぐ試す手順と、自分の `config.toml`・HTML を使う quickstart が出るようにする。
  詳細は既存の文書へリンクする。

## 今の状態（v0.8.2）

- イメージに入っている設定（`docker/assets/conf.d/config.toml`）は TLS の `0.0.0.0:443` と、https への 301 だけを返す
  `0.0.0.0:80` である。
  - 証明書（`/etc/veil/ssl/*.pem`）はイメージに入っていない。
  - そのため設定を渡さずに起動すると、TLS の証明書が無くて止まる。80 番に繋いでも 301 しか返らない。
- この設定は `tools/container_security/` の試験が `-v` で渡して使っている（TLS・seccomp・Landlock の試験）。
- ghcr.io のパッケージのページには説明が出ない。
  - GitHub は `org.opencontainers.image.description` の注釈（テキストだけ、512 文字まで）を名前の下に出す。
  - マルチアーキのイメージでは、index の注釈に付ける必要がある。
  - README は出ない（GitHub の文書「Working with the Container registry」）。

## 設計

### 公開イメージ専用の既定の設定

- `packaging/container/config.toml` を新しく作る。`docker/assets/conf.d/config.toml` は試験用としてそのまま残す。
  - リスナー: `listen_cleartext = "0.0.0.0:8080"`（平文の HTTP/1.1 + h2c）、`enable_secure_redirect = false`。
    TLS のリスナーは置かない（証明書が要らない）。
  - `[[route]]` の `/` → `/var/www/`（`File`。メソッドは `GET`・`HEAD`）。
  - アクセスログは標準出力に text で出す（`docker logs` で見える）。
  - セキュリティは今のイメージの設定と同じにする。
    - seccomp（filter）と Landlock は有効。
    - 特権を `nonroot` に落とす。
    - Docker の既定の seccomp で動く（イメージは full-container = epoll）。
  - 8080 にした理由: root でない利用者（`--user`・Kubernetes の `runAsNonRoot`）でも bind できる。
- `packaging/container/www/` を新しく作る。
  - `index.html` は今の `docker/assets/www/index.html` と同じ作りにする。違いは、quickstart への小さなリンクを足すことだけ。
  - `quickstart.html` を足す。英語で、次のことを書く。
    - すぐ試す方法
    - 自分の HTML を配る方法（`-v ./site:/var/www:ro`）
    - 自分の `config.toml` を使う方法（`-v ./config.toml:/etc/veil/conf.d/config.toml:ro`）
    - TLS・HTTP/3 の方法（証明書をマウントし、`443/tcp`・`443/udp` を開ける）
    - リバースプロキシの例
    - 詳細の文書へのリンク（GitHub の `docs/guide/`）
- release に使う 4 つの Dockerfile（`docker/Dockerfile.{musl,glibc}{,.aarch64}`）の runtime の段で、上の 2 つをコピーする。
  - `EXPOSE` は `8080/tcp 80/tcp 443/tcp 443/udp`。
  - `.dockerignore` に `!packaging/container/` を足す。
- 設定を `-v` で上書きする今の使い方（`docker/README.md`・Kubernetes の ConfigMap）は変わらない。パスは同じ
  `/etc/veil/conf.d/config.toml` である。

### ghcr.io のページ

- `tools/release/publish-images.sh` の `docker buildx imagetools create` に、index の注釈を付ける。
  - `org.opencontainers.image.description`: 512 文字以内の英語。すぐ試すコマンドと、quickstart の文書の URL を書く。
  - `org.opencontainers.image.source`・`org.opencontainers.image.licenses` も付ける。
- Dockerfile の `LABEL` にも同じ description を付ける。単一アーキで pull したときと、Docker Hub の表示のためである。
- 文書: `docs/guide/container.md`（英日）を足す。
  - 内容は 2 つ。すぐ試す方法と、quickstart（自分の設定・HTML・TLS・リバースプロキシ・Kubernetes）。
  - 詳細は `configuration.md`・`running.md`・`tls.md`・`kubernetes.md`・`security.md`・`docker/README.md` へリンクする。
- `docs/guide/README.md`（英日）の目次、README（英日）の Quickstart にリンクを足す。

## 試験

- `tools/release/image-smoke.sh` に `defaults` の場合を足す。設定をマウントせずに起動し、次のことを確かめる。
  - `GET /` が 200 で、内蔵の `index.html` を返す。
  - `GET /quickstart.html` が 200。
  - h2c でも 200 が返る。
  - `docker logs` にアクセスログの行が出る。
  - Docker の既定の seccomp と、io_uring を拒否する seccomp の両方で確かめる。
- release.yml のイメージの試験（rc のタグ）で回す。
- 公開の後、ghcr.io から pull し、`docker run` だけで `/` が返ることと、パッケージのページに説明が出ることを確かめる（F-245）。

## 関連

- [F-237](F-237-publish-container-images.md)（公開イメージ）
- [F-244](F-244-library-default-access-log.md)（ライブラリの既定）
- [F-245](F-245-version-0-8-3.md)
