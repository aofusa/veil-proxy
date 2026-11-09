# B-350: Windows の UDP ソケットの bind の失敗が「os error 0」になる

**状態: 修正済み（feat/v0120-perf、2026-11-09）**

v0.12.0 の Windows の検証（ライブラリの E2E の L4 UDP）で、`bind` の失敗のログが `Bad file descriptor` でも `WSAEACCES` でもなく
`The operation completed successfully. (os error 0)` だったことから見つけた。

## 事象

Windows で UDP の `bind` が失敗すると（ポートの予約・使用中など）、ログと返るエラーが `os error 0` になり、原因が分からない。
L4 の UDP プロキシと、API の L4 UDP ハンドラが通る `runtime::udp::UdpSocket::bind`。

## 原因

失敗の経路で `closesocket(sock)` の後に `WSAGetLastError` を読んでいた。`closesocket` の成功がスレッドの最後のエラーを 0 に書き換える。

## 対応

`closesocket` の前に `last_wsa_error()` で読む（失敗の経路 2 箇所）。ほかの Windows の `closesocket` の経路（`reactor/tcp/windows.rs`）は
先に読んでいた（監査済み）。

## テスト

単体: `runtime::udp::tests::a_failed_bind_reports_the_os_error`（手元に無いアドレスへの `bind`。`os error 0` にならないこと。全 OS）。

結果: Windows 実機（aofish）で単体 1145 件が通った（`60fc2a63`。[platform_verification.md](../../perf/platform_verification.md)）。
