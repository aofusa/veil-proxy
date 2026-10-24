# Veil Linux パッケージ（.deb / .rpm）

Debian/Ubuntu 向け `.deb` と Amazon Linux 2023 向け `.rpm` を生成・検証するためのディレクトリです。

## 概要

| 項目 | 内容 |
|------|------|
| パッケージ形式 | `.deb`（Debian/Ubuntu）、`.rpm`（Amazon Linux 2023）、スタンドアロン `.tar.gz`（glibc / musl バイナリ） |
| ビルドスクリプト | `packaging/scripts/build.sh`（deb / rpm / 両バイナリ tar.gz を一括生成） |
| ライブラリ成果物 | `packaging/scripts/build-lib.sh`（C ライブラリ・Python wheel・Rust ソースを `packaging/output/lib/veil-lib-*.zip` 1 個に。F-195） |
| デフォルトビルド | `--features full`（全オプションフィーチャー有効） |
| 設定ファイル | `packaging/defaults/`（`tools/release/render-defaults.sh linux-package` で作る。F-247） |
| 設定配置先 | `/etc/veil/config.toml`（veil の既定のパス。deb の conffile・rpm の `%config(noreplace)`。F-251） |
| systemd ユニット | `contrib/systemd/veil.service` |
| 実行ユーザー | `veil:veil` |

## ビルド時の落とし穴（実際に踏んだもの）

### NetBSD aarch64 の `dist` プロファイルはメモリ不足で失敗しうる

`[profile.dist]`（`lto="fat"` + `codegen-units=1`）の最終リンクは大量のメモリを要求する。
NetBSD aarch64 VM（既定 `VM_MEM_MB=4096`）では **`rustc` が SIGKILL（OOM）される**。

```
process didn't exit successfully: `rustc --crate-name veil ... -C lto=fat -C codegen-units=1 ...` (signal: 9, SIGKILL: kill)
```

対処（いずれか）:
- `VM_MEM_MB=8192` 以上でリトライする
- それでも足りなければ `CARGO_PROFILE_DIST_LTO=thin` を付けて thin LTO に落とす（`bsd-vm.sh` がゲストの cargo へ渡す）
- `CARGO_PROFILE=release` でビルドする（LTO 無しのぶんバイナリは大きくなるが機能は同一）

### ビルドの失敗をパイプで握り潰さないこと

```bash
# 悪い例: $? は tail のものになり、ビルド失敗が EXIT=0 として記録される
tools/qemu/bsd-vm.sh netbsd aarch64 build 2>&1 | tail -4; echo "EXIT=$?"

# 良い例: ログはファイルへ、終了コードは直接受け取る
tools/qemu/bsd-vm.sh netbsd aarch64 build > build.log 2>&1; echo "EXIT=$?"
```

実際にこれで **OOM 失敗を「成功」と記録し、古いバイナリのまま packaging してしまった**
（`verify-artifacts.sh` の内容検証で発見）。

### x86_64 BSD ゲストは Apple Silicon 上では TCG（実測値）

Apple Silicon の macOS で `bsd-vm.sh <os> x86_64` を回すと x86_64 は TCG
エミュレーションになる。**遅いが実用可能**であることを 2026-08-28 に実測した。

| 段階 | FreeBSD 14.3 amd64 | OpenBSD 7.9 amd64 | NetBSD 10.1 amd64 |
|---|---|---|---|
| 初回起動（SSH 到達まで） | 約 15 分 | インストール込みで約 40 分 | 約 10 分 |
| toolchain（pkg / pkg_add / pkgin） | 約 10 分 | 約 10 分 | 約 10 分 |
| `--profile dist` ビルド | 約 1 時間 | **約 1 時間 20 分**（うち fat LTO のリンクだけで約 50 分） | **約 2 時間 30 分**（wasm 込みで wasmtime/cranelift のコンパイルが重い） |
| 生成物 | 25,751,432 B | 23,582,896 B | 23,491,400 B |

**fat LTO のリンク中は rustc が一切出力しない**ため、ログの最終行が数十分変わらないのは
正常である（`ps` で QEMU が 100% CPU を維持しているかで生死を判断する。
リンクは単一スレッドなので 200% → 100% に落ちたらリンク段階に入った合図）。

ディスクは 1 VM ずつ作って取得後に `disk.qcow2` を消せば **20〜30GB の空きで回せる**。

### TCG（x86_64 ゲスト on Apple Silicon）では provision の SSH 待ちを延ばす

`bsd-vm.sh <os> x86_64 provision` の SSH 到達待ちは既定 **1200 秒**。
Apple Silicon 上の x86_64 ゲストは TCG エミュレーションなので、
**インストール自体は成功しているのに、インストール後の初回起動が 1200 秒に間に合わず
provision が失敗扱いになる**ことがある（OpenBSD 7.9 で実際に発生。
コンソールには `CONGRATULATIONS! Your OpenBSD install has been successfully completed`
が出ているのに `ERROR: SSH 1200s` で終了する）。

タイムアウトは秒数で上書きできる（引数はディスパッチャが転送する）:

```bash
tools/qemu/bsd-vm.sh openbsd x86_64 provision 3600
```

### OpenBSD の autoinstall は GROW_GB を小さくすると失敗する

`GROW_GB=20` にしたところ、OpenBSD 7.9 の autoinstall が `base79.tgz` の取得中に

```
sha256: stdout: write error: No space left on device
Fetching of base79.tgz failed. Continue anyway? [no] no
```

で失敗した（qcow2 は 592MB しか伸びていない = ホストのディスクではなくゲストの
パーティションが原因）。OpenBSD の «(A)uto layout» はディスク全体を固定比率で分割するため、
ディスクを小さくすると `/usr` も比例して小さくなる。**既定の `GROW_GB=24` を下回らないこと。**

### 4 コア機で VM と docker build を同時に走らせないこと

QEMU VM 3 台 + `docker build` を並行させると BuildKit が
`frontend grpc server closed unexpectedly` で落ちる。**直列で回すこと。**

## 成果物の鮮度検証（必ず実施すること）

`docker build` は **呼び出した時点**のソースをビルドコンテキストとして送る。
複数ターゲットを直列に流している最中にソースを変更すると、
「先に始まったターゲットは古いソース、後から始まったターゲットは新しいソース」
という**混在状態**になる。しかも成果物のタイムスタンプはどちらも新しくなるため、
`ls` で眺めても古いバイナリを見分けられない。

そこで、直近の変更で追加した**設定キー名やログのフォーマット文字列**を指定して
成果物の中身を検査するスクリプトを用意している。

```bash
# 例) [http3] recv_drain_max を追加した変更（F-152）が全成果物に入っているか
./packaging/scripts/verify-artifacts.sh recv_drain_max
```

`OK` / `STALE` を成果物ごとに列挙し、1 つでも `STALE` があれば終了コード 1 を返す。
**リリース前には必ず実行し、`STALE` が無いことを確認すること。**

## ディレクトリ構成

```
packaging/
├── README.md                    # 本ファイル
├── debian/DEBIAN/               # .deb メタデータ
├── rpm/veil.spec                # .rpm spec ファイル
├── scripts/
│   ├── build.sh                 # 統合ビルド（.deb + .rpm）
│   ├── build-bsd.sh             # FreeBSD/OpenBSD/NetBSD tar.gz（VM ネイティブビルド）
│   ├── build-cross.sh           # macOS / Windows / FreeBSD tar.gz・zip（Docker クロスビルド）
│   ├── test-install.sh          # 両パッケージを順に検証
│   ├── test-deb.sh              # .deb 検証
│   ├── test-rpm.sh              # .rpm 検証
│   ├── postinstall.sh           # deb/rpm 共通インストール後処理
│   ├── preuninstall.sh          # deb/rpm 共通アンインストール前処理
│   └── docker/
│       ├── Dockerfile.test-deb  # Ubuntu テスト用イメージ
│       └── Dockerfile.test-rpm  # Amazon Linux 2023 テスト用イメージ
├── build/                       # Docker ビルド中間成果物（.gitignore）
├── staging-deb/                 # .deb ステージング（.gitignore）
├── rpm/{BUILD,BUILDROOT,RPMS}/  # rpmbuild 作業ディレクトリ（.gitignore）
└── output/                      # 生成パッケージ（.gitignore）
```

## 前提条件

### ローカルビルド

| ツール | 用途 |
|--------|------|
| `cargo` | veil バイナリビルド |
| `cmake`, `nasm` | `full` フィーチャー（[docs/guide/ja/building.md](../docs/guide/ja/building.md) 参照） |
| `dpkg-deb` | `.deb` 生成 |
| `rpmbuild` | `.rpm` 生成（`rpm` パッケージ） |

Debian/Ubuntu:

```bash
apt-get install -y cmake nasm dpkg-dev rpm
```

### Docker ビルド（推奨）

ホストに Rust ツールチェーンや dpkg-deb, rpmbuild がなくても、Docker だけでビルドできます。
`docker/Dockerfile.glibc` / `docker/Dockerfile.musl` で各バイナリをビルドし、コンテナ内で deb/rpm を作成、あわせてスタンドアロン tar.gz を `packaging/output/` へ出力します。

```bash
# パッケージ一式を Docker 内でビルド
./packaging/scripts/build.sh --docker
```

### テスト

- Docker
- ネットワーク（初回イメージ取得）

## ビルド

### 推奨: Docker でポータブルビルド（glibc 2.28 互換 + musl）

Debian/Ubuntu と Amazon Linux 2023 の両方で動作する glibc バイナリを生成します。
[docker/Dockerfile.glibc](../docker/Dockerfile.glibc)（`messense/cargo-zigbuild`）と
[docker/Dockerfile.musl](../docker/Dockerfile.musl)（`messense/rust-musl-cross`）を使用します。
deb/rpm には glibc バイナリを同梱し、両 libc 向けのスタンドアロン tar.gz も出力します。

```bash
./packaging/scripts/build.sh --docker
```

### ローカル（ネイティブ cargo / cargo zigbuild）

```bash
./packaging/scripts/build.sh
```

- **glibc**: `cargo zigbuild` が利用可能な場合は glibc 2.28 向けに自動ビルドします。
  利用できない場合はホスト glibc でビルドされ、Amazon Linux 2023 では動作しない可能性があります。
- **musl**: `cargo zigbuild --target <musl>`、または `cargo build --target <musl>`
  （`RUST_TARGET_MUSL`、既定は `x86_64-unknown-linux-musl`。要 musl ターゲット）。

### 既存バイナリからパッケージのみ生成

```bash
cargo build --release --features full
./packaging/scripts/build.sh --skip-build --binary target/release/veil
# musl の tar.gz も同時に出す場合:
# ./packaging/scripts/build.sh --skip-build \
#   --binary target/x86_64-unknown-linux-gnu/release/veil \
#   --binary-musl target/x86_64-unknown-linux-musl/release/veil
```

### aarch64（arm64）向けパッケージ（F-120 Phase 3/6）

`RUST_TARGET` に aarch64 ターゲットを指定すると、`ARCH` が自動的に `aarch64` へ
追従し deb は `arm64`・rpm は `aarch64` として出力される。

```bash
# aarch64 の .deb / .rpm / tar.gz（Docker クロスビルド）
RUST_TARGET=aarch64-unknown-linux-gnu ./packaging/scripts/build.sh --docker
```

#### `docker build --platform` はコンパイルターゲットではない

**aarch64 バイナリを生成するのは専用 Dockerfile
（`docker/Dockerfile.{glibc,musl}.aarch64`）と `RUST_TARGET` である。**
どちらの Dockerfile も **ビルダーは x86_64** で、その上で aarch64 ELF を
クロスコンパイルする（glibc は cargo-zigbuild、musl は rust-musl-cross）。

`docker build --platform` が決めるのは次の 2 つだけで、バイナリの ISA は決めない。

1. ピンしていない全 `FROM` の既定プラットフォーム
2. 最終イメージの OCI `Architecture` メタデータ

そのため glibc と musl で扱いが **非対称**になる。

| | `docker build --platform` | 理由 |
|---|---|---|
| glibc | **付けない** | ランタイムが `FROM --platform=${RUNTIME_PLATFORM} cgr.dev/chainguard/glibc-dynamic` なので Dockerfile 内だけで最終イメージが arm64 になる |
| musl | **付ける** | ランタイムが `FROM scratch`（空・アーキ非依存）で、Dockerfile 内だけでは OCI Architecture を確定できない |

> **glibc に `--platform linux/arm64` を付けてはならない。** ビルダーの
> `messense/cargo-zigbuild` まで arm64 として解決され、**rustc 一式が QEMU
> エミュレーション上で動いて極端に遅くなる**（実測で、本来 x86_64 ネイティブなら
> 十数分で終わる依存ビルドが数時間規模になった）。防御として
> `Dockerfile.glibc.aarch64` のビルダーも `--platform=${BUILDER_PLATFORM}`
> （既定 `linux/amd64`）にピンしてある（musl は当初からピン済み）。

`docker create` は「できあがった arm64 イメージ」から作るので、glibc / musl とも
`--platform linux/arm64` が必要（`build.sh` が build 用と create 用を分けて渡す）。

なお packaging の `--docker` は **イメージを実行しない**（`docker create` +
`docker cp` で `/veil` を取り出すだけ）。できた aarch64 パッケージは実 aarch64 機向けで、
既定 feature は `full`（io_uring）。x86_64 上の QEMU user-mode では io_uring の
syscall が `ENOSYS` になるため動かない。コンテナとして試すなら
[docker/README.md](../docker/README.md) の `full,epoll` 手順を参照。

### NetBSD 対応の現状（F-140）

NetBSD 向けの feature セット（`full-netbsd`、TLS プロバイダの
target 別分岐、kqueue reactor の `struct kevent` 型差吸収）は **コード側は完成済み**。
`tools/qemu/bsd-vm.sh`（`netbsd` os として追加）・`packaging/scripts/build-bsd.sh`・
`packaging/bsd/netbsd/veil.rc` の組み込みも完了しており、以下の FreeBSD/OpenBSD 節と
同じインタフェースで NetBSD も扱える。2026-07-29 に NetBSD 10.1 amd64 の実機（QEMU）で
setup/provision まで確認済み。詳細・既知の不確実点は
[`docs/backlog/features/F-140-netbsd-support.md`](../docs/backlog/features/F-140-netbsd-support.md)
と [`tools/qemu/README.md`](../tools/qemu/README.md) を参照。

> **NetBSD バイナリも Proxy-Wasm が使える（B-55 解消）**: crates.io wasmtime（36 / 40 とも）の
> シグナルベーストラップ実装（`signals.rs`）には NetBSD 向けの `ucontext` 分岐が
> 一切無く、実機（NetBSD 10.1 amd64）で確認したところ **x86_64 ですら**
> `compile_error!("unsupported platform")` でビルドできなかった（FreeBSD/OpenBSD は
> aarch64 のみ非対応、NetBSD はアーキテクチャ不問で非対応）。これを解消するため
> crates.io wasmtime 36.0.17（LTS、B-79 で 40.0.4 から移行）を `third_party/wasmtime`（パッケージ名のみ
> `veil-wasmtime`）として vendoring し、対象ターゲット（NetBSD 全アーキ・FreeBSD/
> OpenBSD aarch64）だけ `build.rs` の `has_native_signals` を強制的に `false` にした
> ものを Cargo のターゲット別依存で選択させ、常に Pulley インタープリタで実行する
> ようにした。`full-netbsd` は `wasm` を含む。それ以外の
> 全プラットフォームは crates.io の wasmtime をそのまま使い無影響。詳細は
> [`docs/backlog/bugs/B-55-wasmtime-no-bsd-aarch64.md`](../docs/backlog/bugs/B-55-wasmtime-no-bsd-aarch64.md)・
> [`../third_party/wasmtime/README.veil.md`](../third_party/wasmtime/README.veil.md)。

> **NetBSD で WASM を実行するには `paxctl +m` が必須（B-60、実機 NetBSD 10.1 で確認）**:
> NetBSD は PaX MPROTECT をシステム全体で強制しており
> （`security.pax.mprotect.enabled`/`.global` = 1）、Pulley インタープリタ実行でも
> wasmtime のランタイム `mmap`/`mprotect` が `EACCES` で失敗し `on_request_headers`
> 等のホスト関数呼び出しが軒並みエラーになる（OpenBSD の wxallowed/MAP_STACK・
> B-52 の NetBSD 版に相当）。`/usr/sbin/paxctl +m <veil バイナリ>` で明示的に
> MPROTECT 制限を解除する必要がある。`tests/e2e_setup.sh`（NetBSD 実行時に自動）・
> `tools/qemu/bsd-vm.sh`（`cmd_build` がビルド直後に自動適用）・
> `packaging/scripts/build-bsd.sh`（NetBSD ホストでのパッケージ化時は自動適用、
> それ以外は生成される `INSTALL.txt` に手順を明記）に組み込み済み。詳細は
> [`docs/backlog/bugs/B-60-netbsd-pax-mprotect-wasm.md`](../docs/backlog/bugs/B-60-netbsd-pax-mprotect-wasm.md)。

### FreeBSD / OpenBSD / NetBSD 向けパッケージ（F-120 Phase 6 / F-140）

FreeBSD/OpenBSD/NetBSD のバイナリは **QEMU VM 内でネイティブビルド**したものを取り出し、
専用スクリプトで rc.d サービススクリプト・設定リファレンス・（FreeBSD は）jail.conf
サンプルを同梱した tar.gz を生成する（deb/rpm は Linux 専用のため BSD は tar.gz のみ）。

VM の作成からビルド・E2E・バイナリ取得までは
[`tools/qemu/bsd-vm.sh`](../tools/qemu/README.md) が **FreeBSD/OpenBSD/NetBSD ×
x86_64/aarch64 の 6 通り**を同じインタフェースで面倒を見る（x86_64 ゲストはホストに
`/dev/kvm` があれば KVM 加速される）。

```bash
# setup → provision → toolchain → build → e2e → fetch を一括
# （6 通り: freebsd|openbsd|netbsd × x86_64|aarch64。すべて同じ形）
tools/qemu/bsd-vm.sh freebsd x86_64 all
tools/qemu/bsd-vm.sh freebsd aarch64 all
tools/qemu/bsd-vm.sh openbsd x86_64 all
tools/qemu/bsd-vm.sh openbsd aarch64 all
tools/qemu/bsd-vm.sh netbsd x86_64 all
tools/qemu/bsd-vm.sh netbsd aarch64 all

# 取り出したバイナリ（packaging/build/veil-<os>-<arch>）を tar.gz 化。
# --from-qemu は .os-version も自動で拾うので --binary / --os-version は不要。
./packaging/scripts/build-bsd.sh --os freebsd --arch x86_64 --from-qemu

# 取得済みのものをまとめて（存在する組み合わせだけ処理する）
./packaging/scripts/build-bsd.sh --all
```

**NetBSD 固有の注意（F-140）**:

- **両アーキとも**配布されている起動可能な生イメージ（x86_64 = `-live.img.gz`、
  aarch64 = `evbarm-aarch64/binary/gzimg/arm64.img.gz`）をそのまま qcow2 化して使う。
  cloud-init 相当が無いため `provision` はシリアルコンソールへ root ログインして
  SSH 鍵を注入する（`tools/qemu/netbsd-provision.py`、FreeBSD の `--mode login` と
  同じ発想）。aarch64 は当初 install ISO の `sysinst` をシリアル自動操作していたが、
  実機で言語選択メニューのまま停止するため廃止した。
- **x86_64 のルートディスクは拡張してはいけない（B-82）。** live イメージは
  MBR + BIOS 経路のため、qcow2 の仮想サイズを変えると CHS ジオメトリが変わって
  ルートパーティションの位置がずれ、起動時の fsck が `UNEXPECTED INCONSISTENCY`
  で失敗する。ビルドに必要な容量は `setup` が作る 2 台目のディスク
  （`scratch.qcow2` → ゲストの `ld1`）で与え、`provision` が `/work` にマウントして
  `/usr/pkg`・`/var/db/pkgin`・`GUEST_ROOT`・`CARGO_HOME`・`TMPDIR` をそこへ逃がす。
  aarch64 は UEFI + GPT でこの CHS 依存が無いため、従来どおり拡張する。
- `toolchain` は **`rust-bin`**（バイナリパッケージ）を pkgin で導入する
  （`rust`（ソースビルド）は QEMU 上で数時間かかるため避ける）。
- x86_64 は Rust 1.96.0、aarch64 は Rust 1.91.1 が pkgsrc から入手できる
  （veil の MSRV を満たすか要確認）。

段階を分けて実行することもできる（失敗時はその段階から再開できる）:

```bash
tools/qemu/bsd-vm.sh freebsd x86_64 setup      # イメージ取得 + シード/応答ファイル生成
tools/qemu/bsd-vm.sh freebsd x86_64 provision  # SSH 鍵注入まで
tools/qemu/bsd-vm.sh freebsd x86_64 toolchain  # rust / cmake / llvm / gmake / protobuf
tools/qemu/bsd-vm.sh freebsd x86_64 build      # --no-default-features --features full-freebsd
tools/qemu/bsd-vm.sh freebsd x86_64 e2e        # tests/e2e_setup.sh test
tools/qemu/bsd-vm.sh freebsd x86_64 fetch      # → packaging/build/veil-freebsd-x86_64
tools/qemu/bsd-vm.sh freebsd x86_64 reset      # 初期状態へ（再ダウンロード不要）
```

**所要時間の実測**（4 コア / KVM 有効ホスト、x86_64 ゲスト）:

| 対象 | in-VM リリースビルド |
|---|---|
| FreeBSD 14.3 amd64（`full-freebsd`） | **約 30 分** |
| OpenBSD 7.9 amd64（`full-openbsd`） | **約 72 分** |

aarch64 ゲストは x86_64 ホストでは TCG なので数倍〜十数倍かかる。
**OS ごとに固有の落とし穴がある**（OpenBSD は `/` が ~628M しかない、BoringSSL が
OpenBSD を想定しておらず cc ラッパ・libstdc++ 互換リンクが要る、など）。
`bsd-vm.sh` がすべて自動で処理するが、内容と検証状況は
[tools/qemu/README.md](../tools/qemu/README.md) にまとめてある。

対象 OS の VM 内で `build-bsd.sh` を直接実行する場合は `--os-version` 省略で
`uname -r` から自動検出される。

### プラットフォーム別の feature セット（`full-freebsd` / `full-openbsd` / `full-netbsd` / `full-macos` / `full-windows`）

BSD・macOS・Windows 向けパッケージは `full` ではなく **プラットフォーム専用の feature セット**でビルドする
（`Cargo.toml` の `[features]`）。機能セットは `full` と同一で、**アロケータと
FreeBSD 専用 I/O 経路だけが異なる**。cargo にはターゲット別の default features が
無いため、packaging のスクリプト側で `--no-default-features` と併せて明示指定する。

| セット | アロケータ | 追加 | 使う場所 |
|---|---|---|---|
| `full`（既定） | veil-alloc（F-183。Windows では default の mimalloc） | — | Linux |
| `full-macos` | **システムアロケータ**（libmalloc。F-184） | — | `build-cross.sh --target macos` |
| `full-windows` | mimalloc（F-184） | — | `build-cross.sh --target windows` |
| `full-freebsd` | **システムアロケータ**（libc の jemalloc。tikv-jemalloc は B-89 で廃止） | — | `build-cross.sh --target freebsd` / `bsd-vm.sh freebsd …` |
| `full-openbsd` | **システムアロケータ**（`global_allocator` を差し替えない） | 同梱 rustls(ring)/quiche(BoringSSL) | `bsd-vm.sh openbsd …`（既定） |
| `full-netbsd` | システムアロケータ | 同梱 rustls(ring)/quiche(BoringSSL)。**`wasm` を含む**（`third_party/wasmtime` 経由の Pulley 実行、B-55 解消） | `bsd-vm.sh netbsd …`（既定。実 VM 検証は未実施、上記「NetBSD 対応の現状」参照） |

通常の `cargo build --features full` の挙動は従来どおり（unix は veil-alloc、Windows は default の mimalloc、AIO 無効）で変わらない。
`CARGO_FEATURES` 環境変数で上書きもできる。

> **FreeBSD の Docker クロスビルドは削除済み（B-49、未解決のため経路を撤去）**
>
> `docker/Dockerfile.freebsd` と `build-cross.sh --target freebsd` はかつて用意していた。
> Rust のコンパイルまでは通る（`cargo-zigbuild` は FreeBSD libc を同梱し
> `x86_64-unknown-freebsd` は Rust Tier 2）ものの、**リンク段で失敗していた**。
> aws-lc-sys の s2n-bignum アセンブリが FreeBSD クロス構成で 1 つも組み立てられず、
> `undefined symbol: curve25519_x25519_byte` などが多数出る。
> cc ビルダ強制（libssl 非対応）・`CMAKE_SYSTEM_NAME` の調整・`AWS_LC_SYS_NO_ASM`
> （release では禁止）をいずれも試したが未解決で、aws-lc-sys 側の対応が要る。
> 動かない経路を残すと誤用を招くため、Dockerfile ごと削除した。
> 詳細は [B-49](../docs/backlog/bugs/B-49-awslc-freebsd-cross-missing-s2n-bignum-asm.md)。
>
> **FreeBSD は x86_64 / aarch64 とも QEMU VM 内ネイティブビルドが唯一の公式経路**
> （VM 内はネイティブ clang が `.S` を組み立てるため本問題の影響を受けない）:
>
> ```bash
> tools/qemu/bsd-vm.sh freebsd x86_64 build
> tools/qemu/bsd-vm.sh freebsd x86_64 fetch   # → packaging/build/veil-freebsd-x86_64
> ./packaging/scripts/build-bsd.sh --os freebsd --arch x86_64 \
>   --binary packaging/build/veil-freebsd-x86_64
> ```
>
> `./packaging/scripts/build-cross.sh --target freebsd` を実行すると、上記の手順を
> 案内して非ゼロで終了する（黙って未対応値扱いにはしない）。

`aarch64-unknown-freebsd` は Rust Tier 3（prebuilt std 無し）のため、B-49 の解決有無に
かかわらず VM 内ネイティブビルドのみ。

tar.gz には `veil` バイナリ・`rc.d/veil`（サービススクリプト）・`config.toml.default`・
`www/index.html`・`INSTALL.txt`・`BUILD_INFO.txt`（+ FreeBSD は `jail.conf.sample`）を
同梱する。`BUILD_INFO.txt` / `INSTALL.txt` には **ビルドした OS のバージョン**
（例 FreeBSD 14.3-RELEASE / OpenBSD 7.6）・ビルド日時・rustc バージョンを明記する
（ABI 互換の目安。大きく異なる OS バージョンでは再ビルド推奨）。
FreeBSD は capsicum（`[security] enable_capsicum`）・jail と、OpenBSD は
pledge/unveil（`[security] enable_pledge` / `enable_unveil`）と併用できる。
OpenBSD の TLS は rustls ring プロバイダ + quiche 同梱 BoringSSL 構成（F-122）を使い、
`full-openbsd`（HTTP/3 を含む。アロケータはシステム malloc）でのビルドに対応している。
静的配信/プロキシとも HTTPS 200 で動作する（pledge+unveil 有効のまま）。

> **OpenBSD の WASM について（B-52）**: OpenBSD では wasm を wasmtime の
> **Pulley インタープリタ**（`Config::target("pulley64")`）で実行し、インスタンス
> アロケータを **OnDemand**、ファイバスタックを **`MAP_STACK` 付き**で確保する。
> - OpenBSD 6.4+ はスタックポインタが `MAP_STACK` 領域を指すことをカーネルが強制する。
>   wasmtime 既定のファイバスタックは `MAP_STACK` 無しのため、wasm を 1 回呼んだだけで
>   プロセスが SIGSEGV で落ちる。
> - `Config::with_host_stack`（スタック確保の差し替え）は **OnDemand アロケータでしか
>   参照されない**（プーリングでは黙って無視される）ため、OpenBSD だけ OnDemand にする。
> - Pulley はネイティブコードを生成しないため、**`wxallowed` なファイルシステムが不要**。
>   ネイティブ JIT だと OpenBSD の W^X により実行ファイルの置き場所が制約される。
>   代償として wasm の実行速度はインタープリタ相当になる。

### macOS 向けパッケージ（F-125、Docker クロスビルド）

macOS は Docker（`messense/cargo-zigbuild`）で **universal2（x86_64 + aarch64 の
fat binary）をクロスビルド**できる。FreeBSD/OpenBSD と異なり VM ネイティブビルドは
不要。QEMU 実行・実機検証は行っていない（クロスビルドが通ることのみを合格基準と
する。設計は `docs/artifacts/f125_windows_macos_design.md`）。
デフォルトで `--features full-macos`（`full` と同じ HTTP/3 + WASM 含む全機能で、アロケータは
システムの libmalloc。F-184）でビルドされる。

```bash
./packaging/scripts/build-cross.sh --target macos
```

内部では [`docker/Dockerfile.macos`](../docker/Dockerfile.macos)（`messense/cargo-zigbuild`
ベース、`Dockerfile.glibc` と同じ cacher/builder 2 段構成）を
`--target artifact --output type=local` でビルドし、`cargo zigbuild --release --target
universal2-apple-darwin --no-default-features --features full-macos` の成果物を取り出す。2 段構成のため、ソース
変更だけの再ビルドでは `aws-lc-sys` / `boring-sys` の重い C ビルドがレイヤキャッシュ
から再利用される（B-47）。macOS は
rustls の暗号プロバイダに **aws_lc_rs** を使い（`Cargo.toml` の target 別依存、F-131）、
`http3` (quiche) は内蔵 BoringSSL (`boring-sys`) を独立して使用しシンボル分離されている。

tar.gz には `veil` バイナリ（universal2 fat binary）・`config.toml.default`・
`www/index.html`・`INSTALL.txt` を同梱する。macOS ネイティブのセキュリティは
`sandbox_init`（Seatbelt、`[security] enable_sandbox_macos`）。実機検証ができない
ため保守的な最小プロファイル（ネットワーク・ファイル読み取りは無条件許可、
書き込みのみログ/キャッシュディレクトリへ限定）を採用している
（`src/security.rs` の `macos_sandbox` モジュール参照）。

### Windows 向けパッケージ（F-125、v0.6.0、Docker クロスビルド）

Windows は Docker（`messense/cargo-xwin`）で **x86_64-pc-windows-msvc** と
**aarch64-pc-windows-msvc** を個別にクロスビルドできる（FreeBSD/OpenBSD と異なり
VM ネイティブビルドは不要）。QEMU 実行・実機検証は行っていない
（クロスビルドが通ることのみを合格基準とする。設計は
`docs/artifacts/f125_windows_macos_design.md`）。
デフォルトで `--features full-windows`（`full` と同じ HTTP/3 + WASM 含む全機能で、アロケータは
mimalloc。F-184）でビルドされる。

```bash
./packaging/scripts/build-cross.sh --target windows
```

内部では [`docker/Dockerfile.windows`](../docker/Dockerfile.windows)（`messense/cargo-xwin`
ベース、`Dockerfile.glibc` と同じ cacher/builder 2 段構成）を
`--target artifact --output type=local` でビルドし、
`cargo xwin build --release --target <target> --no-default-features --features full-windows` を x86_64/aarch64 両方に
対して実行して、それぞれ zip を出力する。2 段構成により `aws-lc-sys` / `boring-sys` の
C ビルドと xwin の Windows SDK 取得がレイヤキャッシュに残る（B-47）。rustls の暗号プロバイダは **x86_64 / aarch64 ともに aws_lc_rs**
を使用し（`Cargo.toml` の target 別依存、F-131）、`http3` (quiche) には BoringSSL (`boring-sys`) を、
UDP ソケットには Windows Winsock 互換（`QuicUdpSocket`）が適用されている。`l4-proxy` も対応済みである。

zip には `veil.exe`・`config.toml.default`・`www/index.html`・`INSTALL.txt` を
同梱する。Windows ネイティブのセキュリティは Job Object（best-effort、
`[security] enable_job_object_windows`）。`CreateJobObjectW` +
`SetInformationJobObject` でプロセスに最小限のリソース制限
（`ACTIVE_PROCESS=1`、`KILL_ON_JOB_CLOSE`）を適用するのみで、seccomp/Landlock
相当のシステムコールフィルタではない。

**`AWS_LC_SYS_NO_PREFIX` について（B-47）**: `http3` / `full` ビルドで
`aws-lc-sys` と `quiche` のシンボルをどう扱うかは
**`.cargo/config.toml` の `[env]`（ターゲット接尾辞付き変数）が唯一の設定箇所**である
（Linux/FreeBSD = `1`、Windows/macOS/OpenBSD/NetBSD = `0`）。cargo にターゲット別 env の仕組みが
無いため（`[target.<triple>.env]` は黙って無視される）、`aws-lc-sys` が優先して読む
`AWS_LC_SYS_NO_PREFIX_<triple_with_underscores>` を列挙している。
packaging のスクリプトや Dockerfile 側でこの変数を設定してはならない。

**注意**: 各クロスビルドは専用 Dockerfile のビルドコンテキスト内で完結し、
ホストの `target/` を共有しない（`--output type=local` で成果物だけを取り出す）。
そのためホスト側の `cargo build` と同時に実行しても競合しない。

### 成果物

```
packaging/output/veil_<version>_<deb_arch>.deb          # deb_arch: amd64 / arm64
packaging/output/veil-<version>-1.<rpm_arch>.rpm        # rpm_arch: x86_64 / aarch64
packaging/output/veil-<version>-x86_64-unknown-linux-gnu.tar.gz
packaging/output/veil-<version>-x86_64-unknown-linux-musl.tar.gz
packaging/output/veil-<version>-x86_64-unknown-freebsd.tar.gz    # build-bsd.sh（QEMU VM ビルド）
packaging/output/veil-<version>-aarch64-unknown-freebsd.tar.gz   # build-bsd.sh（QEMU VM ビルド）
packaging/output/veil-<version>-x86_64-unknown-openbsd.tar.gz    # build-bsd.sh（QEMU VM ビルド）
packaging/output/veil-<version>-aarch64-unknown-openbsd.tar.gz   # build-bsd.sh（QEMU VM ビルド）
packaging/output/veil-<version>-x86_64-unknown-netbsd.tar.gz     # build-bsd.sh（QEMU VM ビルド、F-140）
packaging/output/veil-<version>-aarch64-unknown-netbsd.tar.gz    # build-bsd.sh（QEMU VM ビルド、F-140）
packaging/output/veil-<version>-universal2-apple-darwin.tar.gz # build-cross.sh --target macos
packaging/output/veil-<version>-x86_64-pc-windows-msvc.zip      # build-cross.sh --target windows
packaging/output/veil-<version>-aarch64-pc-windows-msvc.zip     # build-cross.sh --target windows
```

`<version>` は **`Cargo.toml` の `[package] version` からビルド時に自動取得**され、
ファイル名・deb の `Version:` フィールド（`debian/DEBIAN/control` はプレースホルダ
`__VERSION__` を持つテンプレート）・rpm の `%{veil_version}` マクロへ反映されます。
リリース時に packaging/ 配下のファイルを手動更新する必要はありません。

tar.gz の中身は次の単一ディレクトリです（展開後 `veil` バイナリのみ）:

```
veil-<version>-<target>/
└── veil
```

例:

```
packaging/output/veil_0.9.1_amd64.deb
packaging/output/veil-0.9.1-1.x86_64.rpm
packaging/output/veil-0.9.1-x86_64-unknown-linux-gnu.tar.gz
packaging/output/veil-0.9.1-x86_64-unknown-linux-musl.tar.gz
```

### ビルド処理の流れ

1. glibc バイナリ生成（`cargo zigbuild` / ホスト cargo、または `Dockerfile.glibc`）
2. musl バイナリ生成（`cargo` musl ターゲット、または `Dockerfile.musl`）
3. スタンドアロン tar.gz を `packaging/output/` に出力（glibc / musl 各1）
4. 共通ルートファイルシステムをステージング（glibc バイナリを使用）
   - `/usr/bin/veil`
   - `/usr/share/veil/config.toml.default`
   - `/usr/share/veil/www/index.html`
   - `/usr/share/veil/scripts/{postinstall,preuninstall}.sh`
   - `/lib/systemd/system/veil.service`
5. `dpkg-deb` で `.deb` を生成
6. `rpmbuild` で `.rpm` を生成

## 公開（リリース。F-237・F-238・F-247〜F-254）

タグ `vX.Y.Z` を push すると `.github/workflows/release.yml` が次を行う。版はタグの版で、タグ（`v` を除く）と
`Cargo.toml` の `[workspace.package] version` が違えば何も公開しない。`docs/releases/v<版>.md`（リリースノート）が無くても
止まる（`tools/release/verify-tag.sh`）。

| 公開先 | 中身 | いつ | 鍵 |
|---|---|---|---|
| `ghcr.io/aofusa/veil` | `latest`＝`musl`＝`musl-vX.Y.Z`、`glibc`＝`glibc-vX.Y.Z`（`linux/amd64`・`linux/arm64`、`full-container`） | タグ | `GITHUB_TOKEN` |
| crates.io | `veil-proxy`・`-macros`・`-build`・`-quiche`・`-wasmtime` | タグ | trusted publishing |
| PyPI | `veil-api` の wheel と sdist | タグ | trusted publishing |
| GitHub Release（**draft**） | 全 OS のアーカイブ・deb・rpm・MSI・BSD の ports・`SHA256SUMS`（Sigstore の署名）・出自の証明・リリースノート | タグ | `GITHUB_TOKEN` |
| Docker Hub・Homebrew・apt/dnf・winget・minisign の署名 | `release-downstream.yml` | **人がページを公開したとき** | environment `release` の secrets |

- **成果物（`dist.yml`）:** Linux は `build.sh --docker`（glibc 2.28・musl、x86_64・aarch64、deb・rpm）、macOS・Windows は
  `build-cross.sh`、BSD は VM の中（`tools/release/bsd-guest-build.sh`。x86_64）。各 OS の runner で
  `tools/release/binary-smoke.sh`、deb・rpm は 8 つの配布 × 2 アーキの `test-package.sh` と署名付きのリポジトリ・改ざんの試験、
  Homebrew は `homebrew-test.sh`、MSI は `test-msi.ps1` と winget のマニフェスト。どれかが落ちると draft を作らない。
- **draft（`release-draft`）:** `tools/release/github-draft.sh`。あれば差し替え、公開済みには触れない。rc も draft（`--prerelease`）。
- **公開の後（`release-downstream.yml`）:** 鍵・token が無ければ失敗する（止めるときはリポジトリの変数
  `DOCKERHUB_MIRROR`・`HOMEBREW_TAP`・`PACKAGE_REPOS`・`WINGET`・`MINISIGN` を `off`）。

手順:

```bash
tools/release/bump-version.sh 0.9.0       # 版を揃える（Helm chart も）→ docs/releases/v0.9.0.md を書く → main にマージ
git tag v0.9.0-rc.1 && git push origin v0.9.0-rc.1   # rc で通しを確かめる（draft の pre-release ができる）
git tag v0.9.0      && git push origin v0.9.0        # 本番の draft を作る → 人が確かめて公開する
```

- プレリリースのタグ（`-` を含む）は、`-` より前（`v0.9.0-rc.1` なら `0.9.0`）を `Cargo.toml` の版と比べる。成果物の版は
  `0.9.0`。イメージは版のタグだけ、crates は検証だけ、PyPI は TestPyPI、GitHub Release は draft の pre-release。
- 版のタグのイメージは上書きしない（同じタグがあれば失敗する）。crates は既にある版を飛ばす。

### 最初の 1 回だけ人が行う準備

1. **GitHub**: Settings → Environments に `crates-io`・`pypi`・`release` を作る。
2. **crates.io・PyPI・ghcr.io・Read the Docs**: v0.8.0 で済み（trusted publishing、パッケージの可視性）。
3. **公開の後の配布先**（`release-downstream.yml`。environment `release` の secrets）:

   | secret | 用途 |
   |---|---|
   | `MINISIGN_SECRET_KEY` | アーカイブの minisign の署名（鍵のファイルの中身。公開鍵は `Cargo.toml` の `[package.metadata.binstall.signing]`） |
   | `DOCKERHUB_USERNAME`・`DOCKERHUB_TOKEN` | `docker.io/aofusa/veil`（Read・Write・Delete の権限、そのリポジトリだけ） |
   | `HOMEBREW_TAP_TOKEN` | `aofusa/homebrew-tap` の Contents と Workflows の書き込み（fine-grained。Workflows が無いと tap の CI を置かずに formula だけを push する） |
   | `PACKAGES_TOKEN`・`PACKAGES_GPG_KEY` | `aofusa/veil-packages`（GitHub Pages）の Contents の書き込みと、署名用の副鍵。空のリポジトリでは Pages を設定できないので、最初の公開で main ができた後に Pages を main / (root) にする |
   | `WINGET_TOKEN` | winget-pkgs の fork への push と PR（最初の版は人が `wingetcreate submit` で出す） |

## インストール

[docs/guide/running.md](../docs/guide/running.md#installing)（apt・dnf のリポジトリ、Homebrew、winget、cargo、BSD の ports）。
手元で作ったパッケージを入れるときは:

```bash
sudo apt install ./packaging/output/veil_0.9.1_amd64.deb      # 入れると起動する
sudo dnf install ./packaging/output/veil-0.9.1-1.x86_64.rpm && sudo systemctl enable --now veil
curl http://127.0.0.1:8080/
```

### postinstall が行うこと

| 処理 | 詳細 |
|------|------|
| ユーザー作成 | `veil` ユーザー / グループ（未存在時のみ。rpm は `%pre`） |
| ディレクトリ作成 | `/var/log/veil`, `/var/cache/veil`, `/var/tmp/veil` |
| 設定 | パッケージのファイル `/etc/veil/config.toml`（新しく入れたときに `root:veil 0640`）。0.8.x の `/var/etc/veil/config.toml` があればそれを使い続ける（`/etc/default/veil`・`/etc/sysconfig/veil` に `VEIL_CONFIG`） |
| ページ | パッケージのファイル `/usr/share/veil/www/` |
| サービス | deb は新しく入れたら有効にして起動、上げたら `try-restart`。rpm は起動しない（上げたら `try-restart`） |

TLS の証明書は作らない（既定の設定は `127.0.0.1:8080` の平文）。

## 動作確認

```bash
./packaging/scripts/test-deb.sh                      # ubuntu:24.04（VEIL_TEST_BASE で変える）
./packaging/scripts/test-rpm.sh                      # amazonlinux:2023
./packaging/scripts/test-package.sh deb debian:12 packaging/output/veil_0.9.1_amd64.deb
```

`test-package.sh` は systemd の動くコンテナで、新しく入れる（deb は入れただけで有効・起動、rpm は `enable --now`）→
`/`・`/quickstart.html`・h2c・journald のアクセスログ・既定のパス・`veil` の利用者 → 書き換えて入れ直す（残る）→ 消す
（deb の purge で消える）→ 0.8.x の `/var/etc/veil` からの移行、を確かめる。

## systemd ユニットの設計上の注意

`contrib/systemd/veil.service` の方針:

- **SystemCallFilter なし** — `config.toml` の seccomp / Landlock と競合するため
- **MemoryDenyWriteExecute なし** — `full` ビルドの WASM 実行に必要
- **LogsDirectory / CacheDirectory** — `/var/log/veil`, `/var/cache/veil`
- **ReadOnlyPaths=/var/etc/veil** — 設定・証明書は読み取り専用
- **AmbientCapabilities=CAP_NET_BIND_SERVICE** — 特権ポート（80/443）を `veil` ユーザーでバインド

## トラブルシューティング

```bash
sudo journalctl -u veil --no-pager -n 50
sudo tail -50 /var/log/veil/veil.error-*.log
```

| 症状 | 原因 | 対処 |
|------|------|------|
| `Permission denied`（設定読込） | TLS 鍵の権限 | `key.pem` を `veil:veil` に変更 |
| `Landlock/seccomp failed` | systemd SystemCallFilter 併用 | ユニットから削除（現行版は対応済み） |
| `NAMESPACE` エラー | ReadWritePaths で存在しないパス | LogsDirectory/CacheDirectory を使用（現行版） |
| `rpmbuild: command not found` | rpm 未インストール | `apt install rpm` または Docker ビルドを使用 |

## 関連ファイル

| パス | 役割 |
|------|------|
| [contrib/config/config.toml](../contrib/config/config.toml) | パッケージ用デフォルト設定 |
| [contrib/systemd/veil.service](../contrib/systemd/veil.service) | systemd ユニット |
| [docker/Dockerfile.glibc](../docker/Dockerfile.glibc) | glibc 配布バイナリビルド（x86_64） |
| [docker/Dockerfile.musl](../docker/Dockerfile.musl) | musl 配布バイナリビルド（x86_64） |
| [docker/Dockerfile.glibc.aarch64](../docker/Dockerfile.glibc.aarch64) | Linux aarch64 glibc クロスビルド（x86_64 上の cargo-zigbuild。ビルダーは `linux/amd64` にピン。`docker build --platform` は付けない） |
| [docker/Dockerfile.musl.aarch64](../docker/Dockerfile.musl.aarch64) | Linux aarch64 musl クロスビルド（x86_64 上の rust-musl-cross。ランタイムが `scratch` のため OCI メタデータ用に `docker build --platform linux/arm64` を付ける） |
| [docker/Dockerfile.macos](../docker/Dockerfile.macos) | macOS universal2 クロスビルド（キャッシュ有効） |
| [docker/Dockerfile.windows](../docker/Dockerfile.windows) | Windows x86_64/aarch64 クロスビルド（キャッシュ有効） |
| [tools/qemu/bsd-vm.sh](../tools/qemu/README.md) | FreeBSD/OpenBSD/NetBSD × x86_64/aarch64 の VM ビルド・E2E・バイナリ取得（`<os> <arch> all` で一括） |
| [packaging/scripts/build-bsd.sh](scripts/build-bsd.sh) | 上記の取得物を tar.gz 化（`--from-qemu` / `--all`） |
| [examples/config.toml](../examples/config.toml) | 設定リファレンス |