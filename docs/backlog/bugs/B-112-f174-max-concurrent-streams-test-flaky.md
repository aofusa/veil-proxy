# B-112: `f174_respects_max_concurrent_streams` が高負荷時に失敗する

**状態: 完了（feat/f178-capsicum-config-reload）**

## 事象

FreeBSD 14.3 x86_64（QEMU/KVM）の単体テストで、`http2::upstream_mux::tests::f174_respects_max_concurrent_streams`
が 1 回失敗した（同じコードの直前の実行では成功）。

```
thread '<unnamed>' panicked at src/http2/upstream_mux.rs:1443:64:
called `Result::unwrap()` on an `Err` value: Os { code: 32, kind: BrokenPipe, ... }
thread 'http2::upstream_mux::tests::f174_respects_max_concurrent_streams' panicked at src/http2/upstream_mux.rs:1679:18:
called `Option::unwrap()` on a `None` value
```

## 原因

テストは `H2Mux::start(client, false, 200ms)` でアイドルタイムアウト 200ms の多重化接続を作り、サーバの
SETTINGS（同時ストリーム 1 本）が反映されるのを最大 1 秒待ってから最初のストリームを開く。VM が高負荷で
サーバスレッドの SETTINGS 送信が 200ms 以上遅れると、ストリーム 0 本のまま接続がアイドル切断される。その結果
サーバ側の書き込みは BrokenPipe になり、最初の `open` は `None` を返す。

本体の不具合ではなく、テストの待ち時間とタイムアウトの食い違い。

## 修正

このテストだけアイドルタイムアウトを 2 秒にする（SETTINGS の待ち時間 1 秒より長く、終了を待つ `wait_dead` の上限 5 秒より短く）。
