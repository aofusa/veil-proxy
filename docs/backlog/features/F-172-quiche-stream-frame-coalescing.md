# F-172: HTTP/3 で複数ストリームの STREAM フレームを 1 パケットへ詰める（quiche vendoring）

## 背景

FreeBSD 14.3 aarch64 の perf（`tools/perf/freebsd/run_perf_freebsd.sh`、3B 静的配信）で
`h3_file` だけが対 nginx 0.75〜0.86 に留まっていた。`syscalls_per_req.sh h3_file` で
1 リクエストあたりの syscall を数えると:

| | 送信 | 受信 | 合計 |
|---|---|---|---|
| veil | `sendto` 1.012 | `recvfrom` 0.588 | 1.71 |
| nginx | `sendmsg` 0.219 | `recvmsg` 0.625 | 1.87 |

DTrace で送信サイズの分布を取ると veil の送信はほぼ全数が 64〜127 バイト
（3B 応答 1 本分）で、**複数リクエストが同じデータグラムで届いても応答は 1 本ずつ
別パケットで出ていた**。

## 原因

quiche 0.24.9 の `Connection::send_single` は、STREAM フレームを書いた直後に
`#[cfg(feature = "fuzzing")]` の分岐でだけ `continue`（次のストリームを同じパケットへ）し、
通常ビルドでは `break` する。つまり **1 パケットに STREAM フレームは常に 1 つ**。
大きなボディでは 1 ストリームでパケットが埋まるので問題にならないが、小さい応答を
多数多重化する HTTP/3 では 1 リクエスト = 1 パケット（`sendto` + AEAD + ヘッダ保護）になる。

veil 側の送出ループ（F-151 のダーティ接続処理 → `send_pending_packets`）は既に
イテレーション末尾で 1 回だけ `conn.send()` を回しており、問題は quiche の内部にあった。

## 対応

`third_party/quiche` に crates.io quiche 0.24.9 を vendoring し（`deps/boringssl` と
`examples/` は除外、2.4MB）、ルート `Cargo.toml` の `[patch.crates-io]` で全ターゲットを
差し替えた。差分は `send_single` の `#[cfg(feature = "fuzzing")]` 1 行の削除のみ
（upstream の fuzzing ビルドで常用されている経路）。path 依存になると cargo が lint を
抑制しなくなるため、`c_void_returns` 警告の出る extern 宣言 2 件も修正した（挙動不変）。
詳細・追従手順は `third_party/quiche/README.veil.md`。

## 結果（FreeBSD 14.3 aarch64、3B、h3load `-c64 -m32`）

- `sendto` 1.012 → **0.143/req**、合計 syscall 1.71 → **0.75/req**（nginx 1.87）
- 対 nginx スループット 0.86 → **0.98**（3 ラウンド中央値）

## テスト

単体 988・統合 54・E2E 555（io_uring / epoll 両方）通過。HTTP/3 の E2E
（quinn + h3 クライアント）は多重化ストリームを含む。
