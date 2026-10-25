# B-166: `SO_ATTACH_REUSEPORT_CBPF` がプロセスで最初に作ったリスナーにしか付かない

**状態: 未着手（2026-10-25 起票。F-260 の調査で見つけた。性能の不具合で、正しさには影響しない）**

## 事象

`src/server.rs` `create_listener`（871〜893 行）は、CBPF の振り分け（`[performance] reuseport_balancing = "cbpf"`、B-76）を
プロセス全体で 1 個のフラグ `CBPF_ATTACHED`（`src/system.rs` 773 行）の compare-and-swap で 1 回だけ付ける。
CBPF のプログラムは `SO_REUSEPORT` の **グループ（アドレス）ごと** に付けるものなので、2 個目以降のリスナー
（TLS と平文の両方を開いた設定の片方、`[[l4]]`）は黙ってカーネルの既定のハッシュになる。

## 影響

CBPF を選んだ設定で、TLS と平文を両方開くと片方だけ CPU の局所性が効かない。F-260 で複数のリスナーを開くと影響が広がる。

## 改修案

- フラグをやめ、`create_listener` に「このリスナーのグループで最初の 1 本か」（`thread_id == 0` 相当）を渡して、
  グループごとに付ける。付ける時点はグループの全ワーカーが bind する前でよい（後から参加したソケットはプログラムを引き継ぐ。今のコメントのとおり）。
- 付けられなかったときの `warn!` にリスナーのアドレスを入れる。
- 試験: Linux の統合試験で 2 個のリスナーを CBPF で開き、両方のログに「CBPF reuseport load balancing enabled」が出る。
  性能は交互 A/B（平文 HTTP/1.1 の 3B）で、平文側が TLS 側と同じ振り分けになること。

## 関連

- [F-260](../features/F-260-multiple-listeners.md)・B-76
