# Veil - High-Performance Reverse Proxy Server

Dockerイメージのビルドとコンテナの実行


ビルド
```sh
# glibc版ビルド
docker build -f Dockerfile.glibc -t "veil:glibc" --build-arg CARGO_FEATURES='full' ..

# musl版ビルド
docker build -f Dockerfile.musl -t "veil:musl" --build-arg CARGO_FEATURES='full' ..
```

### `AWS_LC_SYS_NO_PREFIX` について（B-47）

`http3` / `full` ビルドで `aws-lc-sys` と `quiche` のシンボルをどう扱うかは
**`.cargo/config.toml` の `[env]`（ターゲット接尾辞付き変数）が唯一の設定箇所**である。
Dockerfile 側で `ENV AWS_LC_SYS_NO_PREFIX=...` を設定してはならない（設定が二重化し、
ターゲット別の値と食い違ったときに原因が追えなくなる）。

| ターゲット | 値 | 理由 |
|---|---|---|
| Linux / FreeBSD | `1` | quiche と rustls(aws-lc-rs) が同一の aws-lc-sys を共有する（非プレフィックス） |
| Windows / macOS / OpenBSD | `0` | quiche は内蔵 BoringSSL を使うため aws-lc-sys はプレフィックス維持で共存させる |

cargo には**ターゲット別の環境変数設定が存在しない**（`[target.<triple>.env]` は
黙って無視される）ため、`aws-lc-sys` が優先して読む
`AWS_LC_SYS_NO_PREFIX_<triple_with_underscores>` をグローバル `[env]` に列挙している。
詳細は [`docs/backlog/bugs/B-47-...`](../docs/backlog/bugs/B-47-awslc-no-prefix-not-applied-cross-build.md) を参照。

### 非 Linux 向けクロスビルド（macOS / Windows）

`Dockerfile.glibc` と同じ **cacher（依存だけ先にビルド）→ builder（実ソース）** の
2 段構成にしてあるため、ソースだけを変更した再ビルドでは `aws-lc-sys` /
`boring-sys`（quiche 内蔵 BoringSSL）の重い C ビルドがレイヤキャッシュから再利用される。

| Dockerfile | ターゲット | ベースイメージ |
|---|---|---|
| `Dockerfile.macos` | `universal2-apple-darwin`（x86_64 + aarch64 fat） | `messense/cargo-zigbuild` |
| `Dockerfile.windows` | `x86_64-pc-windows-msvc` / `aarch64-pc-windows-msvc` | `messense/cargo-xwin` |

いずれもランタイムステージを持たない（生成物を Linux コンテナで実行できないため）。
最終ステージ `artifact` は scratch にバイナリだけを置くので、`--output type=local` で
取り出す。

```sh
# 例: Windows aarch64 バイナリを ./out へ取り出す
docker build -f Dockerfile.windows --target artifact \
  --build-arg RUST_TARGET=aarch64-pc-windows-msvc \
  --build-arg CARGO_FEATURES=full \
  --output type=local,dest=./out ..
```

通常は `packaging/scripts/build-cross.sh --target {macos|windows}` から
呼び出す（tar.gz / zip 化まで行う）。

**FreeBSD 向けの `Dockerfile.freebsd` は B-49（未解決）により削除した**: aws-lc-sys の
s2n-bignum アセンブリが FreeBSD クロス構成で 1 つも組み立てられずリンクエラーになる
経路を残しておくと誤用を招くため撤去した。`build-cross.sh --target freebsd` を
実行すると案内を出して終了する。`aarch64-unknown-freebsd` はそもそも
Rust Tier 3（prebuilt std 無し）で Docker では扱えない。**FreeBSD は x86_64 / aarch64
とも QEMU VM 内ネイティブビルドが唯一の公式経路**（`tools/qemu/bsd-vm.sh freebsd
<arch> build` → `fetch` → `packaging/scripts/build-bsd.sh`）。
OpenBSD も同様に `tools/qemu/bsd-vm.sh openbsd {x86_64,aarch64}` を使う。

### aarch64-unknown-linux クロスビルド（F-120 Phase 3）

x86_64 ホスト上で aarch64-unknown-linux-{gnu,musl} バイナリをクロスビルドする。
`Dockerfile.glibc.aarch64` は `messense/cargo-zigbuild`（zig cc 経由のクロスリンク）、
`Dockerfile.musl.aarch64` は `messense/rust-musl-cross:aarch64-musl`（同梱クロス
ツールチェーン）を使う。どちらもビルダー自体は x86_64 イメージ上で動作し、
aarch64 バイナリを**実行しない**（x86_64 ビルダー上で aarch64 テストバイナリは
実行できないため、ビルダー内のユニットテスト実行ステップは省略している。
x86_64 ビルドでは通常どおりテストが実行される）。

```sh
# glibc版（aarch64）ビルド（実 aarch64 ハードウェア向け・io_uring バックエンド）
docker build -f Dockerfile.glibc.aarch64 -t "veil:glibc-aarch64" --build-arg CARGO_FEATURES='full' ..

# musl版（aarch64）ビルド（--platform linux/arm64 必須: イメージメタデータを arm64 で記録する）
docker build --platform linux/arm64 -f Dockerfile.musl.aarch64 -t "veil:musl-aarch64" --build-arg CARGO_FEATURES='full' ..
```

x86_64 ホスト上での実行には QEMU user-mode エミュレーション（binfmt_misc に
`qemu-aarch64` が登録済みであること）が必要。`--platform linux/arm64` を付与して
コンテナを起動する。

> **重要（QEMU user-mode と io_uring）**: qemu-aarch64（user-mode）は io_uring 系
> syscall（425 io_uring_setup / 426 io_uring_enter / 427 io_uring_register）を
> 実装しておらず ENOSYS を返す。そのためデフォルト（io_uring バックエンド）ビルドは
> QEMU 上で起動はするもののワーカーが ring 初期化に失敗して要求を処理できない。
> **QEMU で動作させる場合は `--build-arg CARGO_FEATURES='full,epoll'`（F-120 Phase 2 の
> epoll バックエンド）でビルドすること**（seccomp の扱いは次の注意も参照）。
> 実 aarch64 ハードウェアではデフォルト（io_uring）ビルドをそのまま使える。

```sh
# QEMU（x86_64 ホスト）動作確認用ビルド（epoll バックエンド）
docker build -f Dockerfile.glibc.aarch64 -t "veil:glibc-aarch64" --build-arg CARGO_FEATURES='full,epoll' ..
docker build --platform linux/arm64 -f Dockerfile.musl.aarch64 -t "veil:musl-aarch64" --build-arg CARGO_FEATURES='full,epoll' ..
```

> **重要（QEMU user-mode と seccomp プロファイル）**: QEMU user-mode 実行では、
> コンテナの seccomp プロファイルが制限するのは **エミュレータ
> （qemu-aarch64-static、x86_64 プロセス）自身** の syscall であり、veil 用の
> 最小 allowlist では qemu が必要とする syscall（membarrier 等）まで塞いで即 abort する。
> QEMU 実行時は `--security-opt` を付けず Docker デフォルトプロファイルで動かすこと。
> カスタムプロファイル（`seccomp.json` / `seccomp-epoll.json`、いずれも
> `SCMP_ARCH_AARCH64` 宣言済み）は **実 aarch64 ホスト**で使う。
> 同様に、アプリ内 seccomp/Landlock（`[security]`）も QEMU では適用できない
> （qemu が seccomp(2)/landlock 系 syscall をエミュレートしない）ため、QEMU 検証時は
> 該当設定を無効化するか `allow_security_failures = true` を設定する。

```sh
docker run -it --rm --platform linux/arm64 \
  -p 80:80 -p 443:443 \
  --read-only \
  --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=512m \
  --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=256m \
  -v ./assets/conf.d:/etc/veil/conf.d:ro \
  -v ./assets/ssl:/etc/veil/ssl:ro \
  -v ./assets/www:/var/www:ro \
  --name veil-container-arm64 veil:glibc-aarch64
```

（実 aarch64 ホストでは io_uring ビルド + `--security-opt seccomp=./assets/security/seccomp.json`、
epoll ビルドなら `seccomp-epoll.json` を指定する）

バイナリが aarch64 であることの確認:

```sh
docker run --rm --platform linux/arm64 --entrypoint /usr/bin/file veil:glibc-aarch64 /veil
# => ELF 64-bit LSB pie executable, ARM aarch64, ...
```

E2E（`tests/e2e_setup.sh`）からも `container glibc-aarch64` / `container musl-aarch64`
でこれらのイメージを対象にできる（内部で `docker run --platform linux/arm64` を付与し、
seccomp は Docker デフォルトプロファイルを使う。上記のとおり QEMU では epoll ビルドが前提）。

```sh
./tests/e2e_setup.sh test container glibc-aarch64
```

QEMU user-mode エミュレーション実測（F-120 Phase 3、4 コア x86_64 ホスト）:
フル E2E は約 90 秒で完走し **530/531 通過**。唯一 `test_concurrent_connection_stress`
（200 並行 TLS 接続、クライアント側 5s タイムアウト）はエミュレーションのスループット
不足で恒常的に失敗するため、QEMU 検証の合格基準からは除外する（ネイティブ x86_64 の
epoll ビルドでは同テストを含む 531/531 全通過を確認済み。実 aarch64 ハードウェアでの
確認を推奨）。また QEMU 実行では `-p` ポート公開との組み合わせで応答しない事例があり、
E2E コンテナモードと同じ `--network host` での実行を推奨する。

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
