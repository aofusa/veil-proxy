# B-91: NetBSD でヘルスチェックが落ちたバックエンドを healthy と判定する（std の connect_timeout が拒否を Ok で返す）

## 事象

NetBSD 10.1 aarch64 の VM で統合テストを回したところ（BSD で統合テストを回したのは今回が初）、
`test_f22_tcp_connect_to_closed_port` / `test_f22_tcp_health_check_timeout` /
`test_f18_l4_health_check_excludes_down_backend` が「閉じたポートへの接続が成功した」で失敗した。

VM 内で最小プログラムを動かして確認した:

```
connect_timeout OK; take_error=Ok(Some(Os { code: 61, kind: ConnectionRefused })) peer=Err(NotConnected)
blocking connect: Err(Os { code: 61, kind: ConnectionRefused })
```

**Rust std の `TcpStream::connect_timeout` が NetBSD では拒否された接続に `Ok` を返し、
エラーを `SO_ERROR` に残したままにする**（ノンブロッキング connect 後の `poll(2)` が
`POLLHUP`/`POLLERR` を立てずに `POLLOUT` だけを返すため、std がエラーを拾わない）。

veil の同期接続（`upstream::connect_probe` = HTTP/TCP ヘルスチェックと HTTP/3 の同期 TLS
バックエンド経路、WASM の `proxy_http_call` / `proxy_grpc_*`）はこの API を使っていたため、
**NetBSD では落ちたバックエンドがヘルスチェックで healthy に見えていた**。

## 修正

`upstream::tcp_connect_timeout` を追加し、`connect_timeout` の後に `take_error()` を確認して
`SO_ERROR` が残っていればエラーにする。4 箇所の呼び出しをすべて置き換えた。他 OS では
`take_error()` は常に `None` で挙動は変わらない（専用スレッドの同期経路のみで、データプレーン外）。
統合テストも std を直接呼ばずこのヘルパーを使う（veil の振る舞いを検証する）。

## 付随修正: BSD の単体テスト実行（`bsd-vm.sh unit`）

- NetBSD は PaX MPROTECT がシステム全体で有効（B-60）なので、wasmtime を使う単体テストが
  `Permission denied (os error 13)` で失敗する。E2E は veil 本体に `paxctl +m` を掛けているが、
  単体テストのバイナリは cargo が作るため、cargo の runner（`CARGO_TARGET_<triple>_RUNNER`）で
  実行直前に `paxctl +m` を掛けるようにした。
- 4GB の VM でテストバイナリ 3 本を並列リンクすると ld が OOM になるため `CARGO_BUILD_JOBS=2`。

## 検証

NetBSD 10.1 aarch64（QEMU/HVF）で単体 890・統合 53 が全通過（修正前は統合 3 件失敗、
PaX 対応前は単体 4 件失敗）。
