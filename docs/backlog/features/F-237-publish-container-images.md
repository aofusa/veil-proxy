# F-237: コンテナイメージの公開（musl が主・glibc が副、タグの push で自動公開）

**状態: 完了（feat/v080-release、2026-10-16。公開そのもの〈タグの push〉は main へのマージの後）**

## 目的

veil のコンテナイメージをレジストリに公開する。musl 版を主、glibc 版を副とし、git のタグを push したら CI が自動で
ビルドし、試験してから公開する。

## タグ

タグ `vX.Y.Z`（例: `v0.8.0`）を push したら、次のイメージのタグを付ける。

| イメージのタグ | 中身 |
|---|---|
| `veil:latest` | musl 版（`musl` と同じダイジェスト） |
| `veil:musl` | musl 版の最新 |
| `veil:musl-vX.Y.Z` | その版の musl 版（例: `musl-v0.8.0`） |
| `veil:glibc` | glibc 版の最新 |
| `veil:glibc-vX.Y.Z` | その版の glibc 版（例: `glibc-v0.8.0`） |

- 版のタグには、git のタグの名前をそのまま使う（`v` を付ける）。
- プレリリースのタグ（`v0.8.0-rc.1` のように `-` を含むもの）では、版のタグ（`musl-v0.8.0-rc.1`・`glibc-v0.8.0-rc.1`）だけを
  付ける。`latest`・`musl`・`glibc` は動かさない。CI の試しに rc のタグを使えるようにするためである。
- 版のタグは上書きしない。既に同じタグがレジストリにあれば、公開せずに失敗させる。
  動くタグ（`latest`・`musl`・`glibc`）だけが付け替わる。

## 設計

### レジストリ

- 主のレジストリは GitHub Container Registry の `ghcr.io/aofusa/veil` にする。
  - リポジトリは公開されているので、公開イメージを無料で置ける。
  - workflow の `GITHUB_TOKEN`（`packages: write`）で push できるので、鍵の管理が要らない。
  - パッケージの可視性は、初回の公開の後に public にする（GitHub のパッケージの設定。人が 1 回行う）。
- Docker Hub にも置く場合は、`DOCKERHUB_USERNAME`・`DOCKERHUB_TOKEN` の secret があるときだけ、同じダイジェストを
  `docker.io/<user>/veil` にも付ける。secret が無ければ飛ばす。

### アーキテクチャとビルド

- 対象は `linux/amd64` と `linux/arm64` の 2 つで、マルチアーキのマニフェストにまとめる。
- 使う Dockerfile:

  | 版 | amd64 | arm64 |
  |---|---|---|
  | musl | `docker/Dockerfile.musl` | `docker/Dockerfile.musl.aarch64`（x86_64 からのクロス） |
  | glibc | `docker/Dockerfile.glibc` | `docker/Dockerfile.glibc.aarch64`（zigbuild のクロス） |

- 各イメージは、ダイジェストで push してから `docker buildx imagetools create` でマニフェストにまとめ、タグを付ける。
- arm64 のクロスビルドは、Dockerfile の中で `cargo test --lib` を省いている（QEMU を使わないため）。そこで、arm64 の
  イメージはネイティブの arm のランナー（`ubuntu-24.04-arm`。公開リポジトリは無料）で起動の試験をする。
- キャッシュ: buildx の `cache-to: type=gha,mode=max`。cacher の段（依存のビルド）が効く。musl 版はキャッシュが無いと
  30 分ほどかかる（このホストで計測）ので、ジョブの上限は 120 分にする。

### イメージの中身（io_uring と seccomp）

- 今の Dockerfile の既定の feature は `full`（io_uring）。Docker の既定の seccomp と Kubernetes の `RuntimeDefault` は
  io_uring を拒否するので、そのままでは `docker run` で起動できない環境が多い。
- 採る案: 1 つのイメージに 2 つのバイナリを入れる。
  - `/veil` は `full`（io_uring）、`/veil.epoll` は `full-container`（epoll）。
  - `/veil` は起動時に io_uring が使えなければ、`/veil.epoll` を `execve(2)` する（同じ引数・環境変数）。
  - io_uring が使えるホスト（`--security-opt seccomp=unconfined`・独自のプロファイル）では io_uring で動く。
  - 切り替えたことは、起動時に warn で 1 回出す。
- この切り替えは、F-227（メッシュの io_uring 版）で設計したものと同じ。F-227 の段階 0 の結果を待たずに、
  一般の機能として `src/entry.rs` に入れる。
  - 入れる位置: スレッド・ソケットを作る前。
  - cfg: `veil_rt_uring` のときだけ。
  - 環境変数 `VEIL_FALLBACK_BIN` で切り替え先を変えられる。
  - メッシュも同じ関数を使う。
- イメージの大きさは、2 つのバイナリで増える。増分を測って `docker/README.md` に書く。
  - 増分が大きすぎる場合（既存のイメージの 2 倍を超える場合）の代わりの案: 主のタグを `full-container` の 1 本にし、
    io_uring 版を `musl-uring-vX.Y.Z` として別に出す。そのときはタグの表に足し、利用者向けの文書で選び方を説明する。
- OCI のラベルを付ける: `org.opencontainers.image.source`・`version`・`revision`・`licenses`・`description`。
- 署名と出所:
  - `docker/build-push-action` の `provenance: mode=max`・`sbom: true` を付ける。
  - cosign のキーレス署名（GitHub の OIDC）を付ける。
  - 既存の `release-sbom.yml`（リリースの公開時に SBOM を添付）は、公開したイメージのダイジェストで SBOM を作るように
    つなぎ直す。

### workflow（`.github/workflows/release.yml`、F-238 と共有）

1. `verify`:
   - タグと `cargo metadata` の版が一致すること（F-231）。
   - タグのコミットが main に含まれていること（`git merge-base --is-ancestor`）。
2. `image` の行列（musl・glibc × amd64・arm64）:
   - ビルドし、ダイジェストで push する。
3. `image-test` の行列（各イメージ、arm64 は arm のランナー）:
   - `docker run` の既定の seccomp で起動し、`/veil.epoll` に切り替わることを確かめる。
   - `--security-opt seccomp=unconfined` では io_uring のまま動くことを確かめる。
   - HTTP/1.1・HTTP/2・HTTP/3（UDP）で応答を確かめる。
   - `veil --version` が版と一致することを確かめる。
4. `image-publish`（試験が全部通ったときだけ）:
   - マニフェストを作ってタグを付け、署名し、出所の情報を付ける。
- 権限は、ジョブごとに最小にする（`contents: read`、`packages: write`・`id-token: write` は publish だけ）。
- 外部の action は、コミットの SHA で固定する（F-236 の供給網の観点）。

## 受け入れ条件

- タグ `v0.8.0-rc.1` で、workflow を最後まで通す。rc の版のタグだけが付き、`latest` が動かないことを確かめる。
- そのあと `v0.8.0` で、表の 5 つのタグが付く。`latest` と `musl` のダイジェストが同じになる。
- 公開したイメージを、このホストで次の 3 つの環境から取ってきて起動できる。
  - Docker の既定の seccomp: epoll に切り替わる。
  - seccomp を外した場合: io_uring のまま動く。
  - kind の Kubernetes（`RuntimeDefault`）。
- `docker/README.md`・README の Quickstart（`docker run ghcr.io/aofusa/veil`）・`docs/guide/building.md`・`kubernetes.md`（英日）を
  更新する。
- 既存のプロキシの性能が退行しない（切り替えの確認は起動時の 1 回だけ。`suite_ab.sh`）。


## 実装の結果

- `src/runtime/fallback.rs`: 起動の最初に io_uring を作れなければ `<自分のパス>.epoll`（または `VEIL_FALLBACK_BIN`）を
  同じ引数で `execve`。`docker/Dockerfile.{musl,glibc}{,.aarch64}` は `CARGO_FEATURES_FALLBACK` が空でなければ `/veil.epoll` も入れる。
- `.github/workflows/release.yml`（actionlint・shellcheck 済み）と `tools/release/{verify-tag,image-smoke,publish-images}.sh`。
- 手元の確認（musl、x86_64、`--features full` + `full-container`、52.7MB）: `tools/release/image-smoke.sh` が全部 ok
  （io_uring を拒否する seccomp で epoll 版へ切り替わる、seccomp 無しで io_uring のまま、どちらも HTTP/1.1・2・3、`--version` 0.8.0）。
  kind の Pod（`seccompProfile: RuntimeDefault`）でも「Switching to the epoll build」を出して応答した。
- このホストの Docker（29.8）は既定の seccomp で io_uring を許していた。そこで試験は io_uring を拒否するプロファイルを明示する。
- 実際の ghcr への公開・arm64 のランナーでの試験は、タグを push したときに CI で行う（`packaging/README.md` の「公開」）。

## 関連

F-231（版の検査）、F-227（同じ切り替え）、F-236（公開の前の監査）、F-238（同じ workflow）、`docker/`、`release-sbom.yml`。
