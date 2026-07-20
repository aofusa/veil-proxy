# tools/qemu — aarch64 full-system QEMU 検証環境（F-128）

veil の **io_uring バックエンド**を実 aarch64 カーネル上で E2E / `tools/perf` 検証する
ための QEMU 環境を用意するスクリプト群。

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
