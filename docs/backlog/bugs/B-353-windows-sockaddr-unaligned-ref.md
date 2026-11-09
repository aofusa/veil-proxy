# B-353: Windows の `storage_to_sockaddr` が整列していないバッファへの参照を作る

**状態: 修正済み（feat/v0120-perf、2026-11-09）**

B-350 の調査の途中で読んで見つけた。

## 事象

`runtime/udp.rs` と `runtime/reactor/tcp/windows.rs` の Windows 用 `storage_to_sockaddr` は、`[u8; 128]`（整列 1）のバッファを指すポインタから
`&SOCKADDR_IN` を作っていた。整列要件を満たさない参照を作ること自体が未定義動作で、デバッグビルドの「misaligned pointer dereference」の検査で
abort し得る（x86_64・aarch64 の実機では読めるので、これまで表面化していなかった）。

## 対応

`std::ptr::read_unaligned` で値として読み出す。アドレスファミリもバイト列から読む。

## テスト

単体（Windows のみ）: `runtime::udp::tests::storage_to_sockaddr_reads_an_unaligned_buffer`（1 バイトずらしたバッファから IPv4・IPv6 を読む）。

結果: Windows 実機（aofish）で通った（`60fc2a63`）。
