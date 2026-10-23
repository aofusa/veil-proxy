# F-253: BSD の ports（FreeBSD ports・OpenBSD ports・NetBSD pkgsrc）

**状態: 設計（feat/distribution-tickets、2026-10-23）**

## 目的

- 各 OS の仕組みで、ソースからビルドして入れられるようにする。
  - FreeBSD: `pkg install veil` / `cd /usr/ports/www/veil && make install`
  - OpenBSD: `pkg_add veil`
  - NetBSD: pkgsrc の `www/veil`
- 入れた直後に、その OS のサービスの仕組みで既定の設定で起動し、`curl http://127.0.0.1:8080/` で確かめられるようにする。
  - FreeBSD: `sysrc veil_enable=YES && service veil start`
  - OpenBSD: `rcctl enable veil && rcctl start veil`
  - NetBSD: `/etc/rc.conf` に `veil=YES`
- 各 OS のサンドボックス（capsicum・pledge/unveil・chroot + 降格）が、既定の設定で有効になる。

## 今の状態

- `packaging/bsd/{freebsd,netbsd,openbsd}/veil.rc` と、FreeBSD の `jail.conf.sample` がある。
  - 設定の場所: FreeBSD は `/usr/local/etc/veil/`。OpenBSD・NetBSD の rc は `/etc/veil/`。
    NetBSD は pkgsrc の慣習（`/usr/pkg/etc/`）と違う。
- **バイナリ:** `build-bsd.sh` が VM でビルドする（手元。v0.8.0 まで手で添付）。
- **port の雛形:** ports・pkgsrc の雛形は無い。
- **ビルドで知っている罠:**
  - FreeBSD: `jemalloc` feature は使えない（B-89）。`aio` は使わない（B-63）。
  - OpenBSD: TLS は `ring`、WASM は Pulley + OnDemand（B-52）。
  - NetBSD: 全アーキで vendoring 版の wasmtime（B-55）。実行には `paxctl +m` が要る（B-60）。fat LTO のリンクはメモリを食う。
  - `third_party/`（wasmtime・quiche・quinn-udp）はワークスペースの中の path 依存。
    GitHub のタグの tarball には入るので、ports の distfile はタグの tarball + crates にする。

## 設計

### 共通

- **雛形の置き場所:** veil-proxy の中に置く。
  - `packaging/bsd/ports/freebsd/www/veil/`
  - `packaging/bsd/ports/openbsd/www/veil/`
  - `packaging/bsd/ports/netbsd/www/veil/`
  - 各 OS の ports の木へ出すのは、この雛形を VM で試験してから。
- **feature:** `full-freebsd`・`full-openbsd`・`full-netbsd`。
- **既定の設定:** F-247 の雛形から作る。
  - リスナーは `127.0.0.1:8080` の平文、ページは `share/veil/www/`。
  - root で起動し、`www`（FreeBSD・OpenBSD は `_veil` を作るのが ports の慣習なら、それに合わせる）へ降格する。
- **設定ファイルの扱い:**
  - `config.toml.sample` を置き、無ければ `config.toml` を作る。
    FreeBSD は `@sample`、OpenBSD は `@sample`、NetBSD は `CONF_FILES`。
  - 上げたときに、利用者の設定を上書きしない。
- **rc:** 今の `veil.rc` を ports の慣習に合わせて直す。
  - 置き場所は、FreeBSD が `%%PREFIX%%/etc/rc.d/veil`、OpenBSD が `/etc/rc.d/veil`、NetBSD が `RCD_SCRIPTS`。
  - `reload`（SIGHUP）と `configtest`（`veil -t`）を持たせる。
- **Rust の版:** 各 ports の木の rust の版が、veil のビルドに足りるか確かめる。足りないときは、出す時期を待つ。
  `rust-version` は今は書いていない。最低の版を測って `Cargo.toml` に書くかは、ここで決める。

### FreeBSD

- `USES=cargo`。`CARGO_CRATES` は `make cargo-crates` で作る。
- `CARGO_FEATURES=full-freebsd`、`CARGO_INSTALL_PATH=.`。
- `USE_RC_SUBR=veil`、`USERS`/`GROUPS`、`pkg-plist`。
- 試験: FreeBSD 14.3 の VM（amd64・aarch64。aarch64 は ssh llm の VM）で、次のことを回す。
  - `poudriere testport`
  - 入れた後に `service veil start` → `curl`
  - capsicum が有効なことのログ
- 出す先: FreeBSD の Bugzilla（Ports & Packages）に、新しい port として出す。メンテナは aofusa。

### OpenBSD

- `MODULES=devel/cargo`。`MODCARGO_CRATES` は `make modcargo-gen-crates` で作る。
- `MODCARGO_FEATURES=full-openbsd`。
- pledge・unveil の既定を有効にする（今の rc の注記を、既定の設定に入れる）。
- 試験: OpenBSD 7.9 の VM で、次のことを回す。
  - `make test package`、`pkg_add` で入れる
  - `rcctl start veil` → `curl`
  - pledge の違反が無い
- 出す先: ports@openbsd.org へ、tarball で出す。

### NetBSD（pkgsrc）

- `.include "../../lang/rust/cargo.mk"`。`CARGO_CRATES` は `make print-cargo-depends` で作る。
- `CARGO_FEATURES=full-netbsd`。
- **PaX MPROTECT:** wasmtime に要る `paxctl +m` は、pkgsrc の `NOT_PAX_MPROTECT_SAFE+= bin/veil` で入れたときに付ける（B-60）。
- **ビルドの重さ:** fat LTO のリンクのメモリが要る。`CARGO_PROFILE` は `release` か thin LTO にし、`packaging/README.md` の測定に合わせる。
- 試験: NetBSD 10.1 の VM（x86_64・aarch64）で、次のことを回す。
  - `make package install`
  - rc で起動 → `curl`
  - WASM のフィルタが動く（PaX の確認）
- 出す先: まず pkgsrc-wip に出し、後で pkgsrc の本体へ出す。

### 試験の自動化

- F-247 の `release-bsd.yml`（VM の workflow）に、`ports` のジョブを足す。
  - 雛形の版・チェックサムをタグに合わせて作り直す。
  - 各 OS で、ビルド → 入れる → 起動の試験を回す。
- 雛形の更新（`CARGO_CRATES` など）は `tools/release/bsd-ports-bump.sh` で作る。
  - 各 ports の木への更新の提出は、人が行う。

### 文書

- README（英日）の「Install」に、各 OS の入れ方を書く。
- `docs/guide/platforms.md`（英日）に、次のことを書く。
  - サンドボックスの既定
  - ports の雛形の場所
  - 木に入るまでの、雛形からの入れ方

## 受け入れ条件

- 3 つの OS の VM で、雛形から作ったパッケージを入れると、サービスの仕組みで起動し `curl http://127.0.0.1:8080/` が内蔵のページを返す。
  - その OS のサンドボックスが有効になっている。
- 各 OS の検査（`portlint`・`make test`・pkglint）が通る。
- 各 ports の木への提出（FreeBSD の Bugzilla・ports@・pkgsrc-wip）をした。取り込まれるかは相手の審査に依る。

## 依存・リスク

- F-247（既定の設定の雛形・内蔵のページ）が先。バイナリの成果物には依らない（ソースからビルド）。
- VM の試験が要る（ssh llm の VM・手元の QEMU）。
- 審査の時間は読めない。木への取り込みの前は、雛形と GitHub Release の tar.gz で使ってもらう。

## 関連

- [F-247](F-247-distribution-foundation.md)
- B-52・B-55・B-60・B-63・B-89
- `packaging/bsd/`
