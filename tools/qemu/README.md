# tools/qemu — full-system QEMU 検証環境（プラットフォーム×arch ビルド・動作確認）

veil を **各プラットフォーム×arch の実カーネル上**でビルド・E2E・`tools/perf` 検証する
ための QEMU 環境を用意するスクリプト群。Linux io_uring（実 aarch64 カーネル）に加え、
v0.6.0 では **FreeBSD arm64（aarch64）のネイティブビルド + 動作確認**をカバーする。

ホストに `qemu-system-*` / UEFI ファーム / cloud image ツールが無く sudo も使えない環境を
想定し、これらを内包した Docker ヘルパイメージ（`helper/Dockerfile`）経由で起動する。

## プラットフォーム×arch 検証マトリクス

| プラットフォーム | x86_64 | aarch64 | このディレクトリの手段 |
|---|---|---|---|
| Linux（io_uring/epoll） | ネイティブ/Docker で直接 | full-system QEMU（実カーネル要、`aarch64-vm.sh` + `run-e2e-aarch64.sh`）。**KVM 不可ホストでは TCG が実用不能**（下記制約） | `aarch64-vm.sh` |
| FreeBSD | amd64 は KVM で高速な x86_64 VM（本リポジトリ外の運用 VM。kTLS/capsicum の E2E に使用） | **FreeBSD arm64 VM-IMAGE は TCG でも実用起動**。in-VM ネイティブビルド + smoke（`fbsd-arm64-vm.sh`） | `fbsd-arm64-vm.sh` / `fbsd-arm64-provision.py` / `fbsd-arm64-smoke.sh` / `fbsd-capmode-e2e.sh` |
| OpenBSD | amd64 は KVM で x86_64 VM（pledge/unveil の検証） | ready な VM-IMAGE が無く miniroot からの対話 install + Tier3 で TCG では非現実的（未対応） | — |
| macOS / Windows | — | ネイティブ実行ホストが無くクロスビルド + packaging のみ（`packaging/scripts/build-cross.sh`） | — |

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
tools/qemu/run-e2e-aarch64.sh          # クロスビルド + VM E2E スモーク
```

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
  + `PermitRootLogin yes` を行う（`bsd-arm64-provision.py --mode ssh`）。
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
| `fbsd-arm64-vm.sh` | ライフサイクル（setup/up/grow/provision/smoke/ssh/down） |
| `bsd-arm64-provision.py` | single-user 経由の SSH 鍵注入（`--mode ssh`）/ growfs（`--mode grow`） |
| `fbsd-arm64-smoke.sh` | VM 内 HTTPS 静的配信 smoke（`veil` 起動 → curl → 200 判定） |
| `fbsd-capmode-e2e.sh` | capsicum capability mode 静的配信 E2E（F-123。arch 非依存で amd64/arm64 とも） |

### 環境変数

- `WORKDIR`（既定 `~/qemu-images/fbsd-aarch64`）・`IMG`・`SSH_PORT`(2223)・`CON_PORT`(2224)・
  `KEY`（既定 `~/.ssh/veil_qemu_key`）・`GROW_GB`(20)・`HELPER_IMG`(`veil-qemu-aarch64:local`)。
