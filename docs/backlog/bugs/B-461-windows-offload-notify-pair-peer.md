# B-461: Windows のオフロードの完了通知のソケットの対が、一時リスナーへ最初に接続した相手を確かめずに使っていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Windows offload wake-up channel accepts the first loopback connection without checking it is its own client」から確認。

## 事象

- Windows には eventfd・pipe2 が無いので、`runtime::offload::windows_notify_pair` は `127.0.0.1:0` の一時リスナーへ
  自分で接続し、受理した接続を完了通知の読み取り側にしていた。一時リスナーには同じ端末の他のプロセス（他の利用者）も
  接続でき、先に接続した相手の接続を受理すると、その相手が通知を握り、ワーカーのオフロード（ファイルの読み取り・
  DNS・ハンドラ）の完了を止めたり、偽の通知で空回りさせたりできた（CPython の CVE-2024-3219 と同じ）。

## 対応

- 受理した接続の相手のアドレスが自分の client のローカルアドレスと同じものだけを使い、他は閉じて受理を続ける
  （自分の接続は connect が返った時点でバックログにある。上限は 64 件）。

## テスト

- Windows の単体試験のオフロードの経路（`runtime::offload` の試験）が通ることを aofish の実機で確かめた。他の
  プロセスに先に接続させる試験は、一時ポートの番号を先に知る手段が無いので置いていない。
