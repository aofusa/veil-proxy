# F-33: HTTP/3 UDP GSO/GRO オフロード

## 出典

`docs/artifacts/architecture_analysis_v3.md`（UDP パケットの GSO/GRO 活用）。

## 概要

HTTP/3（quiche）の UDP 送受信で GSO（Generic Segmentation Offload）/ GRO を活用し、
パケットごとのカーネル遷移コストを削減してスループットを数倍化する。

## 現状

- `config.toml` / `[http3]` に `gso_gro_enabled` フラグが存在するが、独自 io_uring ランタイムの
  UDP 送受信（`sendmsg`/`recvmsg`）で GSO/GRO の segment size 制御が完全にはパイプライン化
  されていない。

## 改修内容

1. `src/runtime/` に `IORING_OP_SENDMSG` / `IORING_OP_RECVMSG` の Future を追加し、
   `msghdr` + `cmsg`（`UDP_SEGMENT` / `UDP_GRO`）で一括送受信する。
2. quiche の `send`/`recv` ループを、複数 QUIC パケットを 1 回の sendmsg(GSO) でまとめて送る形に変更。
   受信は GRO で集約したデータグラムを quiche に供給する。
3. `gso_gro_enabled` でランタイム切替（非対応カーネルは単発 sendmsg にフォールバック）。

## 受け入れ条件

- [ ] HTTP/3 E2E が回帰なし。
- [ ] GSO 有効時に同一スループットでの sendmsg 呼び出し回数が削減される。

## 依存・リスク

- `IORING_OP_SENDMSG`/`RECVMSG` は `PROXY_ALLOWED_OPCODES` への追加が必要（seccomp/uring 制限の更新）。
- カーネル/NIC の GSO/GRO 対応差異が大きく、フォールバック設計が必須。
