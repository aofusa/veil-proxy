# tools/qemu — full-system QEMU 検証環境（プラットフォーム×arch ビルド・動作確認）

veil を **各プラットフォーム×arch の実カーネル上**でビルド・E2E・`tools/perf` 検証する
ための QEMU 環境を用意するスクリプト群。

ホストに `qemu-system-*` / UEFI ファーム / cloud image ツールが無く sudo も使えない環境を
想定し、これらを内包した Docker ヘルパイメージ（`helper/Dockerfile`）経由で起動する。
ヘルパには **aarch64 ゲスト（qemu-system-arm + AAVMF）と x86_64 ゲスト
（qemu-system-x86 + OVMF）の両方**が入っており、x86_64 ゲストはホストに `/dev/kvm` が
あれば **KVM 加速**される（実用速度で in-VM フルビルド + E2E が回る）。

## プラットフォーム×arch 検証マトリクス

| プラットフォーム | x86_64 | aarch64 | 手段 |
|---|---|---|---|
| Linux（io_uring/epoll） | ネイティブ/Docker で直接 | Docker クロスビルド + full-system QEMU で E2E（`linux-aarch64-e2e.sh`）。**KVM 不可ホストでは TCG が実用不能**（下記制約） | `aarch64-vm.sh` / `run-e2e-aarch64.sh` / `linux-aarch64-e2e.sh` |
| FreeBSD | Docker クロスビルド（`docker/Dockerfile.freebsd`）または VM 内ネイティブビルド。VM は **KVM で実用速度** | Rust Tier 3 のため **VM 内ネイティブビルド**。TCG でも実用起動する | `bsd-vm.sh freebsd {x86_64,aarch64}` |
| OpenBSD | miniroot から autoinstall した VM で **ネイティブビルド**。KVM で実用速度 | 同左（TCG のため低速） | `bsd-vm.sh openbsd {x86_64,aarch64}` |
| macOS / Windows | ネイティブ実行ホストが無く **Docker クロスビルドのみ**（`docker/Dockerfile.{macos,windows}` / `packaging/scripts/build-cross.sh`） | 同左 | — |

**なぜ Linux/macOS/Windows に QEMU が要らないか**: Linux x86_64 はホストそのもの、
macOS/Windows は Docker（cargo-zigbuild / cargo-xwin）で完結するクロスビルドのみを
合格基準としているため。Linux aarch64 だけは io_uring を**実カーネル**で確認したいので
QEMU を使う。

---

## BSD 統合ヘルパ `bsd-vm.sh`（FreeBSD / OpenBSD × x86_64 / aarch64）

`packaging/` が配布する BSD バイナリを**実 OS 上でビルド・E2E 検証**し、成果物を
host 側へ取り出す（`packaging/scripts/build-bsd.sh` へ渡す）ための統合スクリプト。

```bash
tools/qemu/bsd-vm.sh <os> <arch> <command> [args]
#   os   : freebsd | openbsd
#   arch : x86_64 | aarch64
```

| コマンド | 内容 |
|---|---|
| `setup` | helper イメージ build + ゲストイメージ取得 + SSH 鍵生成 |
| `up` / `down` / `status` / `console` | VM ライフサイクル |
| `wait` | SSH 到達までブロック |
| `grow` | ディスク拡張（FreeBSD: `qemu-img resize` + single-user `growfs`） |
| `provision` | SSH 鍵注入 + sshd 有効化（FreeBSD はシリアル single-user 経由 / OpenBSD は autoinstall） |
| `toolchain` | VM 内へ rust / cmake / llvm / bash / curl を導入 |
| `sync` | リポジトリを VM へ転送（tar over ssh） |
| `build` | VM 内でリリースビルド（既定 `--no-default-features --features full-freebsd\|full-openbsd`） |
| `e2e` | VM 内で `tests/e2e_setup.sh test` を実行 |
| `fetch` | VM 内の release バイナリを `packaging/build/` へ取得 |

### フル一巡（FreeBSD amd64、KVM 有効ホスト）

```bash
tools/qemu/bsd-vm.sh freebsd x86_64 setup
tools/qemu/bsd-vm.sh freebsd x86_64 up
tools/qemu/bsd-vm.sh freebsd x86_64 grow        # root FS は既定 ~5G で不足する
tools/qemu/bsd-vm.sh freebsd x86_64 provision
tools/qemu/bsd-vm.sh freebsd x86_64 wait
tools/qemu/bsd-vm.sh freebsd x86_64 toolchain
tools/qemu/bsd-vm.sh freebsd x86_64 build       # full features
tools/qemu/bsd-vm.sh freebsd x86_64 e2e         # tests/e2e_setup.sh test
tools/qemu/bsd-vm.sh freebsd x86_64 fetch       # packaging/build/veil-freebsd-x86_64
tools/qemu/bsd-vm.sh freebsd x86_64 down

# 取得したバイナリを packaging に渡す
./packaging/scripts/build-bsd.sh --os freebsd --arch x86_64 \
  --binary packaging/build/veil-freebsd-x86_64 \
  --os-version "$(cat packaging/build/veil-freebsd-x86_64.os-version)"
```

### FreeBSD x86_64 の Docker クロスビルドは現在使えない（B-49）

Zig が FreeBSD の libc を同梱しており `x86_64-unknown-freebsd` は Rust Tier 2 なので
`docker/Dockerfile.freebsd` を用意してあるが、**aws-lc-sys の s2n-bignum アセンブリが
FreeBSD クロス構成で組み立てられず、リンク段で失敗する**（B-49、未解決）。
当面は x86_64 も上記の VM 内ネイティブビルドを使うこと。

B-49 が解決すれば、クロスビルドしてから VM では **E2E だけ**回す運用にできる
（VM 内フルビルドを省ける）。

```bash
./packaging/scripts/build-cross.sh --target freebsd
tools/qemu/bsd-vm.sh freebsd x86_64 e2e \
  --prebuilt packaging/build/artifact-x86_64-unknown-freebsd/veil
```

`--prebuilt` を渡すと `VEIL_E2E_SKIP_VEIL_BUILD=1` が効き、veil 本体を VM 内でビルド
しない。ただし `grpc_server` / `test_backends` と E2E テストバイナリ自体は cargo が
必要なので VM 内でビルドされる（`toolchain` は必要）。

`aarch64-unknown-freebsd` は Rust Tier 3（prebuilt std 無し）のため Docker では扱えず、
`bsd-vm.sh freebsd aarch64 build` の VM 内ネイティブビルドを使う。

### ファイル一覧

| ファイル | 役割 |
|---|---|
| `bsd-vm.sh` | **FreeBSD/OpenBSD × x86_64/aarch64 の統合ヘルパ**（本節） |
| `freebsd-provision.py` | FreeBSD のシリアル single-user 経由 SSH 鍵注入（`--mode ssh`）/ growfs（`--mode grow`） |
| `openbsd-autoinstall.py` | OpenBSD の autoinstall(8) をシリアルコンソールから駆動 |
| `console-dump.py` | シリアルコンソール（telnet）を非対話で読み出す（`console` サブコマンド） |
| `qmp-sendkeys.py` | QMP `send-key` でゲストへブラインド入力を送る（シリアルが使えない環境の切り分け用）。QMP は `SSH_PORT+2` で公開している |
| `helper/Dockerfile` | qemu-system-{arm,x86} + AAVMF/OVMF + ssh/python3-pexpect を収録したヘルパイメージ |
| `aarch64-vm.sh` | Linux aarch64 VM のライフサイクル |
| `run-e2e-aarch64.sh` | Linux aarch64 の HTTPS スモーク E2E |
| `linux-aarch64-e2e.sh` | Linux aarch64 で `tests/e2e_setup.sh test` の全 E2E を実行 |
| `fbsd-arm64-vm.sh` / `fbsd-arm64-smoke.sh` | FreeBSD arm64 の従来経路（smoke 専用。新規用途は `bsd-vm.sh` を推奨） |
| `fbsd-capmode-e2e.sh` | capsicum capability mode 静的配信 E2E（F-123） |

### 検証状況（重要）

| 項目 | 状況 |
|---|---|
| ゲストイメージ URL（FreeBSD amd64/arm64・OpenBSD 7.9 amd64/arm64） | **HTTP 200 を確認済み**（FreeBSD arm64 の URL は "RELEASE" 二重で 404 だったのを修正。OpenBSD 7.6 は CDN から消えていたため既定を 7.9 へ） |
| `setup`（helper build → イメージ DL → 展開） | **FreeBSD amd64 で実行成功**（helper イメージ build・806MB の xz DL・3.7GB qcow2 展開まで） |
| `up`（KVM 加速つき起動） | **起動成功**（`accel=kvm -cpu host`。コンテナ内 qemu が動作継続） |
| ローダメニューのシリアル操作（Space/`3`/`set console`/`boot -s -h`） | **到達確認済み**（下記参照） |
| `provision` 以降（`grow`/`toolchain`/`build`/`e2e`/`fetch`） | **未達**。カーネルがシリアルへ出力しないため single-user シェルを掴めない。下記「FreeBSD amd64 のシリアルコンソール問題」を参照 |
| `openbsd-autoinstall.py` | **未実行**。OpenBSD インストーラの対話文言に依存するため、初回実行時に応答の追従が要る可能性が高い |
| `linux-aarch64-e2e.sh` | **未実行**（KVM 非対応ホストでは TCG が実用不能。下記「既知の環境制約」参照） |

### FreeBSD amd64 のシリアルコンソール問題（未解決・調査結果）

`provision`（`freebsd-provision.py` がローダメニューから single-user に入り root の
SSH 鍵を注入する経路）は **amd64 では最後まで到達しない**。以下は実測で分かったこと。

**動くところ:**

- `-serial telnet:...,server,nowait` は**接続前の出力を捨てる**。VM 起動から数秒以内に
  コンソールへ接続すれば、**SeaBIOS → boot2 → ローダの出力はシリアルに出る**
  （当初「何も出ない」と見えたのは接続が遅かったため）。`cmd_provision` が
  down→up 直後にスクリプトを起動するのはこのため。
- ローダメニューはシリアルから操作できる。Space で autoboot を止め、`3` で
  ローダプロンプトへ入り、`set console="comconsole"` → `boot -s -h` まで通る
  （コンソール切り替え直後は 1 文字落ちるので空行を挟む必要がある）。

**動かないところ:**

- ローダが `Loading configured modules...` まで出した後、**カーネルはシリアルへ
  一切出力しない**。`console="comconsole"` と `-h`(RB_SERIAL) の両方を与えても同じ。
  `-machine q35` / `-machine pc` の双方で再現する。
  そのため single-user のシェルプロンプトを掴めず、鍵注入まで進めない。

**試して駄目だった代替手段:**

| 手段 | 結果 |
|---|---|
| UEFI(OVMF) で起動して efiboot のシリアル出力を使う | OVMF 自体がシリアルへ出力せず、ローダにも到達を確認できない |
| libguestfs でイメージをオフライン編集し `/boot/loader.conf` に `console="comconsole"` を書く | **UFS が読み取り専用**。アプライアンスの Linux カーネルは `ufstype=ufs2` で mount できるが書き込み不可（`Read-only file system`） |
| QMP `send-key` で VGA コンソールへブラインド入力（`qmp-sendkeys.py`） | ローダメニュー操作までは成功（QMP `screendump` で確認済み）。ただし `set console="comconsole"` を入れるとローダが VGA キーボードを読まなくなり、以降タイプできない |

`qmp-sendkeys.py` と `console-dump.py` は上記の切り分けで実際に役立ったため残してある
（`screendump` でゲスト画面を PNG に落として確認できる）。

**次に試す価値がある案:**

- FreeBSD の `bootonly.iso` / `mini-memstick.img` からシリアル指定でインストールし、
  `console="comconsole"` を含むイメージを一度だけ作って `BASE_IMG` として使い回す。
- ホストで root が使える環境なら `qemu-nbd` でイメージをマウントして
  `/boot/loader.conf` を編集する（本リポジトリの想定外だが最短）。
- arm64（`bsd-vm.sh freebsd aarch64`）は UEFI + efiboot が既定でシリアルを使うため
  この問題を受けない。amd64 のビルドを急ぐ場合は arm64 側で先に流す手もある。

#### `BASE_IMG`: プロビジョニング済みイメージのオーバーレイ運用

```bash
BASE_IMG=~/qemu-images/freebsd-14.3-amd64.qcow2 \
  tools/qemu/bsd-vm.sh freebsd x86_64 setup
tools/qemu/bsd-vm.sh freebsd x86_64 up
tools/qemu/bsd-vm.sh freebsd x86_64 wait
```

`setup` が `BASE_IMG` を backing file とする qcow2 オーバーレイを作るので、
**元イメージは一切変更されない**。起動時は backing を `/base` へ読み取り専用で
マウントし、qemu が backing に共有 write ロックを取ろうとして失敗するのを
`backing.file.locking=off` で回避する。

> 本セッションではこの経路で既存イメージの起動まで確認したが、SSH が
> banner exchange でタイムアウトした（ゲストの NIC 名や rc.conf が、本スクリプトの
> `-machine q35` + `virtio-net-pci` 構成と噛み合っていない可能性がある）。
> **コンソール出力が無いため原因の切り分けができず、build/e2e までは到達していない。**

> OpenBSD の CDN は直近数リリースしか保持しない（7.6 は既に 404）。既定は 7.9。

### ポート割り当て

同時に 4 VM を起動しても衝突しないよう os/arch ごとに固定している
（`SSH_PORT` / `CON_PORT` で上書き可）。

| VM | ssh | console(telnet) |
|---|---|---|
| freebsd x86_64 | 2310 | 2311 |
| freebsd aarch64 | 2320 | 2321 |
| openbsd x86_64 | 2330 | 2331 |
| openbsd aarch64 | 2340 | 2341 |

### OpenBSD の VM 作成（autoinstall）

OpenBSD は FreeBSD と違い ready-made な qcow2 を配布していないため、
`installNN.img`（miniroot）から **autoinstall(8) で無人インストール**して起動ディスクを
作る（`openbsd-autoinstall.py`）。応答ファイルは helper コンテナ内の
`python3 -m http.server` から `http://10.0.2.2:8000/auto_install.conf` として配る
（slirp のゲートウェイ 10.0.2.2 = QEMU を実行しているコンテナ自身）。

amd64 のブートローダは既定で VGA コンソールへ出るため、`boot>` へ
**ブラインドで `set tty com0`** を送ってシリアルへ切り替えている（arm64 は UEFI +
efiboot が既定でシリアルなので不要）。

> **注意**: `openbsd-autoinstall.py` は OpenBSD インストーラの**対話文言に依存**する。
> リリースによって文言が変わった場合は追従が必要。`OPENBSD_VER`（既定 7.9。CDN は直近リリースのみ保持するため古い版は 404 になる）で
> バージョンを指定する。

---

## Linux aarch64（io_uring、`aarch64-vm.sh`）

veil の **io_uring バックエンド**を実 aarch64 カーネル上で E2E / `tools/perf` 検証する。

## なぜ full-system emulation か

- QEMU **user-mode**（`qemu-aarch64`）は `io_uring` の syscall
  （`io_uring_setup`/`io_uring_enter` と mmap 経由の SQ/CQ 共有リング）を正しく
  エミュレートできず、独自 io_uring ランタイムが起動できない。
- そのため **full-system emulation**（`qemu-system-aarch64` + 実 Linux カーネル）を
  使い、ゲスト内で本物の io_uring を動かす。
- epoll バックエンド（`--features epoll`）の aarch64 検証は user-mode QEMU でも可能で、
  そちらは `docker/Dockerfile.{glibc,musl}.aarch64` 側でカバーする。ここは
  **io_uring 専用**の検証環境。

## 前提と制約

- ホストに `qemu-system-aarch64` / UEFI ファーム / `cloud-image-utils` が無く sudo も
  使えない環境を想定し、これらを内包した Docker ヘルパイメージ（`helper/Dockerfile`）
  経由で QEMU を起動する。
- x86_64 ホスト上の aarch64 full-system は **TCG（ソフトウェアエミュレーション）** で
  動くため非常に低速。**VM 内でのフルビルドは避け**、ホストでクロスコンパイルした
  バイナリのみを VM へ転送して実行する。
- **既知の環境制約（重要）**: KVM が使えない（クロスアーチ）ホストでは TCG のみとなり、
  汎用クラウドイメージ（Ubuntu cloud image / Alpine cloud image いずれも）は
  systemd/OpenRC の初期化やサービス依存解決の段階で `hrtimer: interrupt took ...` を
  伴い実用不能なほど遅く（`soft lockup CPU#0 stuck` に至る場合もある）、SSH 到達前に
  停滞することがある。この場合、**フルシステムの対話的 E2E は当該ホストでは成立しない**。
  aarch64 の妥当性確認は次の 2 点で代替する:
  1. 現行コードの **aarch64 クロスビルド成功**（`messense/rust-musl-cross:aarch64-musl`
     で `aws-lc-sys` の bindgen に `BINDGEN_EXTRA_CLANG_ARGS=--sysroot=...` を渡す。
     成果物は実 aarch64 ELF・静的リンク）。
  2. io_uring 経路は **アーキテクチャ非依存**（カーネル io_uring ABI は LE 全アーチで
     同一、SQE/CQE の struct レイアウトも共通）であることのコードレベル論証。

  KVM 対応（ネイティブ aarch64 ホスト or ネスト仮想化）が使えるホストでは、上記
  `up`→`wait`→`run-e2e-aarch64.sh` がそのまま実 io_uring E2E として機能する。

## 使い方

```bash
# 1. 環境準備（ヘルパイメージ build + cloud image DL + cloud-init seed）。初回のみ。
tools/qemu/aarch64-vm.sh setup

# 2. VM 起動（detached）
tools/qemu/aarch64-vm.sh up

# 3. SSH 到達まで待機（TCG のため初回は数分〜数十分）
tools/qemu/aarch64-vm.sh wait

# 4. VM 内でコマンド実行 / ファイル転送
tools/qemu/aarch64-vm.sh ssh 'uname -mr; cat /proc/sys/kernel/io_uring_disabled'
tools/qemu/aarch64-vm.sh scp ./veil /home/veil/veil

# 5. 後片付け
tools/qemu/aarch64-vm.sh down
```

## E2E / perf の一括実行

`run-e2e-aarch64.sh` が「ホストで aarch64-gnu クロスビルド → VM へ転送 → io_uring で
起動 → HTTPS スモーク E2E」を一括で行う。`tools/perf` の aarch64/io_uring 実行にも
同じ VM を使う（`PERF_TARGET=aarch64-qemu` は `tools/perf/run_perf.sh` から本 VM の
SSH 経由でバイナリを起動する）。

```bash
tools/qemu/run-e2e-aarch64.sh          # クロスビルド + VM E2E スモーク（HTTPS 静的配信のみ）
```

より強い検証として、**Docker でクロスビルドした aarch64 バイナリを VM へ持ち込み、
`tests/e2e_setup.sh test` の全 E2E スイートを実 aarch64 カーネル上で回す**スクリプトも
用意している（B-47）。

```bash
tools/qemu/linux-aarch64-e2e.sh        # docker build → VM 転送 → tests/e2e_setup.sh test
```

veil 本体は `docker/Dockerfile.glibc.aarch64` の成果物を使い、VM 内では
`VEIL_E2E_SKIP_VEIL_BUILD=1` でビルドを省略する（`grpc_server` / `test_backends` と
E2E テストバイナリは cargo が要るので VM 内でビルドされる）。上記「既知の環境制約」の
とおり、KVM が使えないホストでは TCG が遅すぎて完走しない。

## VM 仕様

- ゲスト: Ubuntu 24.04 arm64 cloud image（`-machine virt -cpu cortex-a72`）。
- ユーザ: `veil` / パスワード `veil`（SSH は公開鍵認証。鍵は `setup` で自動生成）。
- SSH: ホスト `127.0.0.1:2222`（`SSH_PORT` で変更可）。
- 起動高速化のため cloud-init で `snapd`/`multipathd` を mask する。
- 資材は `~/qemu-images/aarch64/`（`VEIL_QEMU_DIR` で変更可）。

---

## FreeBSD arm64（aarch64、`fbsd-arm64-vm.sh`）

**FreeBSD 14.x arm64 VM-IMAGE は Linux aarch64 と異なり TCG（KVM 不可ホスト）でも実用
起動する**。よって aarch64 の「ビルド + 動作確認」を当環境で実施できる（v0.6.0 で確立）。
aarch64-unknown-freebsd は Rust Tier3（prebuilt std 無し・cross は build-std 要）のため、
**VM 内でネイティブビルド**する（`pkg install rust cmake llvm` → `cargo build`）。

```bash
# 1. helper build + VM-IMAGE DL + 鍵生成（初回のみ）
tools/qemu/fbsd-arm64-vm.sh setup
# 2. VM 起動（telnet シリアルコンソール + hostfwd ssh）
tools/qemu/fbsd-arm64-vm.sh up
# 3. ディスクを +20G 拡張（rust + build に必要。root FS 既定 ~5G では不足）
tools/qemu/fbsd-arm64-vm.sh grow
# 4. single-user 経由で root SSH を有効化（鍵注入 + sshd 有効化）
tools/qemu/fbsd-arm64-vm.sh provision
# 5. source 転送 → in-VM ネイティブビルド → HTTPS 静的配信 smoke
tools/qemu/fbsd-arm64-vm.sh smoke        # 期待: ARM_SMOKE=PASS（HTTP 200）
# 6. 後片付け
tools/qemu/fbsd-arm64-vm.sh down
```

### 落とし穴（project memory / v0.6.0 検証で確認済み）

- `virtio-net-pci` には **`romfile=`（空）** が必須（`efi-virtio.rom` 不足で起動失敗）。
- シリアルに **getty が無く root SSH も既定無効** → `provision` は **loader メニューで
  single-user（"2"）** を選び、getty 不要の root シェルから鍵注入 + `sysrc sshd_enable=YES`
  + `PermitRootLogin yes` を行う（`freebsd-provision.py --mode ssh`）。
- qemu の **telnet シリアルコンソールは IAC(0xff)** を送るため pexpect は `encoding="latin-1"`。
  unix socket は root 所有で非 root が connect できないため **TCP telnet** を使う。
- **root FS が ~5G と小さい**。`qemu-img resize` 後の online growfs はマウント中 root で
  "not clean" 拒否 → **single-user で / を `mount -u -o ro /` → `fsck` → `growfs`**
  （`grow` サブコマンドが gpart resize + fsck + growfs を実施）。
- `pkg` は **`IGNORE_OSVERSION=yes ASSUME_ALWAYS_YES=yes`** で userland 版不一致プロンプト回避。
- aws-lc-sys（FreeBSD は aws_lc_rs provider）の bindgen が **libclang** を要求 →
  `pkg install llvm` + `LIBCLANG_PATH=/usr/local/llvmNN/lib`。
- sshd は TCG で **banner 応答が遅い** → ssh `ConnectTimeout=90` 程度。
- **TCG のクリーンビルドは数時間規模**（aws-lc-sys の C ビルドが律速）。

### スクリプト

| ファイル | 役割 |
|---|---|
| `fbsd-arm64-vm.sh` | ライフサイクル（setup/up/grow/provision/smoke/ssh/down）。**新規用途では `bsd-vm.sh freebsd aarch64` を推奨**（本スクリプトは smoke 専用の従来経路として残置） |
| `freebsd-provision.py` | single-user 経由の SSH 鍵注入（`--mode ssh`）/ growfs（`--mode grow`） |
| `fbsd-arm64-smoke.sh` | VM 内 HTTPS 静的配信 smoke（`veil` 起動 → curl → 200 判定） |
| `fbsd-capmode-e2e.sh` | capsicum capability mode 静的配信 E2E（F-123。arch 非依存で amd64/arm64 とも） |

### 環境変数

- `WORKDIR`（既定 `~/qemu-images/fbsd-aarch64`）・`IMG`・`SSH_PORT`(2223)・`CON_PORT`(2224)・
  `KEY`（既定 `~/.ssh/veil_qemu_key`）・`GROW_GB`(20)・`HELPER_IMG`(`veil-qemu-aarch64:local`)。
