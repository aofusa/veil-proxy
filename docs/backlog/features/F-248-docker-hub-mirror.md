# F-248: Docker Hub へのミラー

**状態: 実装済み（feat/distribution-tickets、2026-10-23。v0.9.0 で公開）**

## 目的

- ghcr.io と同じイメージを `docker.io/aofusa/veil` にも置く。
  - ghcr.io に届かない環境や、Docker Hub を既定とするツール・社内ミラーの利用者が使えるようにする。
  - タグ・digest・署名を ghcr.io と同じにする。
- `docker run -p 8080:8080 aofusa/veil` だけで、既定の設定で起動する（F-246 と同じ）。

## 今の状態

- `release.yml` の `image-publish` に「copy to Docker Hub (only when the secrets exist)」の段がある。
  - `DOCKERHUB_USERNAME`・`DOCKERHUB_TOKEN` があるときだけ、`publish-images.sh` で ghcr.io からコピーする。
- secrets は設定されていない。`hub.docker.com/v2/repositories/aofusa/veil` は 404（2026-10-23）。
  - ユーザー `aofusa` は Docker Hub に存在する。リポジトリの持ち主の本人かどうかは確かめる。
- **黙って飛ばされる:** secrets が無いと段ごと飛ばされ、workflow は成功になる。
  ミラーを始めた後に token が切れたり消えたりしても気づけない（集計の偽 OK と同じ形。B-100〜B-103・B-159）。
- **署名・説明:** cosign の署名は ghcr.io の digest に付けているだけ。Docker Hub 側の説明（README）は無い。

## 設計

### 人が用意するもの（実装の前に）

- Docker Hub で、名前空間 `aofusa` にリポジトリ `veil`（public）を作る。
- その 1 リポジトリだけに Read & Write の権限を持つ、アクセストークンを作る（Organization Access Token か、権限を絞った PAT）。
- GitHub の environment `release` の secrets に `DOCKERHUB_USERNAME`・`DOCKERHUB_TOKEN` を置く。
  repository の secrets ではなく environment に置き、タグの workflow からだけ読めるようにする。

### workflow

- コピーの方法は、今の `publish-images.sh <docker.io/...> ... <ghcr の元>`（`imagetools create` で manifest をそのまま写す）を使う。
  - 4 つのタグ（`latest`・`glibc`・`musl-v<ver>`・`glibc-v<ver>`）とも、digest が ghcr.io と同じになる。
  - rc のタグの扱いも ghcr.io と同じにする（pre-release は版のタグだけで、`latest` を動かさない）。
- **署名:** `cosign copy` で ghcr.io の署名・SBOM の添付を Docker Hub へ写す。
  - 写した後、`cosign verify docker.io/aofusa/veil@<digest>` を、ghcr.io と同じ identity・issuer で確かめる。
- **説明:**
  - `packaging/container/dockerhub.md` を足す。内容はすぐ試す方法、自分の設定・HTML、TLS・HTTP/3、文書へのリンク。
  - `packaging/container/description.txt`（100 文字以内の短い説明）と一緒に、Docker Hub の API で送る。
    外部の action を使うなら、SHA で固定する。
- **黙って飛ばさない:**
  - 本番のタグで secrets が無いときは失敗にする。
  - secrets を設定するまでは、workflow の変数 `DOCKERHUB_MIRROR=off` で明示的に止める。
    ログには「止めている」と出す。

### 公開の後の確かめ

- `docker.io/aofusa/veil:<tag>` を pull し、次のことを確かめる。
  - digest が ghcr.io と同じ。
  - `cosign verify` が通る。
  - `image-smoke.sh defaults` が通る（何も指定せずに `/`・`/quickstart.html`・h2c・アクセスログ）。
- F-245 の公開の後の確かめの台本に、Docker Hub の行を足す。

### 文書

- `docs/guide/container.md`（英日）・`docker/README.md`・README の Quickstart に、Docker Hub の名前を併記する。
- Docker Hub の匿名の pull には回数の制限がある。CI で大量に pull するときは ghcr.io を勧める。

## 受け入れ条件

- 本番のタグの workflow で、4 つのタグが Docker Hub に載る。digest は ghcr.io と同じで、署名の検証が通る。
- Docker Hub のページに説明が出る。
- secrets が無い・無効なときに、本番のタグの workflow が失敗する（黙って成功しない）。

## 依存・リスク

- 人の作業（リポジトリ・トークン・secrets）が要る。
- Docker Hub の無料の名前空間の保持の方針（使われないタグの削除など）を確かめる。

## 関連

- [F-237](F-237-publish-container-images.md)
- [F-246](F-246-container-try-it-defaults.md)
- [F-247](F-247-distribution-foundation.md)
