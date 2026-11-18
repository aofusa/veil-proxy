# B-460: 待ち受けのソケットが SO_REUSEADDR を付けており、同じ端末の他の利用者が同じポートへ重ねて bind できた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Windows listeners set SO_REUSEADDR without SO_EXCLUSIVEADDRUSE, so any local user can bind veil's port and take its
connections」と「L4 UDP sockets set SO_REUSEADDR, letting another local user co-bind the port on Linux and take
datagrams」から確認。

## 事象

- Windows の TCP のリスナー（`runtime::reactor::tcp::windows` の `bind`・`bind_reuse_port`）は `SO_REUSEADDR` を付けて
  ワーカーごとに同じポートへ bind していた。Windows では最初のソケットに `SO_REUSEADDR` があると、`SO_REUSEADDR` を
  付けた他の利用者のソケットも特権なしで同じアドレス・ポートへ bind でき、届く接続がどちらへ渡るかは定まらない。
  平文のリスナーへの要求（Cookie・Authorization）を他の利用者が受け取れた。
- `runtime::udp::UdpSocket::bind`（L4 の UDP のリスナー・上流のセッション）は Linux・Windows で `SO_REUSEADDR` を
  付けていた。Linux の UDP は両方のソケットに `SO_REUSEADDR` があると利用者（UID）を問わず重ねて bind でき
  （`udp_lib_lport_inuse`）、データグラムを奪えた。HTTP/3 の Linux のソケット（`udp::socket` の
  `bind_reuseport_with_gso`）も `SO_REUSEPORT` に加えて `SO_REUSEADDR` を付けていた。

## 対応

- Windows の TCP: 待ち受けのソケットは `SO_EXCLUSIVEADDRUSE` で bind する。同じアドレスの `bind_reuse_port` は
  1 本の待ち受けのソケットを共有し、各ワーカーには `WSADuplicateSocketW` で作った複製を渡す（最後のリスナーが落ちたら
  閉じる）。ポート 0 と `bind` は共有しない。
- UDP: Linux では `SO_REUSEADDR` を付けない（UDP に TIME_WAIT は無い。ワーカー間の共有は同じ UID に限られる
  `SO_REUSEPORT` で行う）。Windows は `SO_EXCLUSIVEADDRUSE`。
- 利用者ガイド（`platforms.md` の英日の Windows の行）に書いた。

## テスト

- Windows の単体試験 `runtime::reactor::tcp::windows::tests::reuse_port_listeners_share_one_exclusive_socket`
  （2 つのワーカーのリスナーが同じソケットを共有してどちらでも受理でき、`SO_REUSEADDR` を付けた他のソケットは
  bind できず、全部閉じたらポートが空く）。aofish の実機で確かめた。
