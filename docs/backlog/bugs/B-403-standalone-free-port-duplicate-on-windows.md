# B-403: 自己完結型 E2E の `free_port` が、Windows で 1 つの試験の 2 つのポートに同じ番号を返すことがあった

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

Windows（aofish、`full-windows`）の自己完結型 E2E で `parity_tests::b285_stalled_continuation_is_reset_on_h2` が、veil の起動時の
設定の検査で落ちた。

```
Config load error: [server].listen and [server].listen_cleartext must differ (127.0.0.1:63633)
```

試験は `parity::Ports::alloc` で平文と TLS のポートを `free_port()` で 1 つずつ取る。2 つが同じ番号だった。

## 原因

`free_port` は `127.0.0.1:0` に bind して得た番号を、ソケットを閉じてから返す。Windows の `bind(0)` は閉じたばかりの番号を
次の `bind(0)` でまた返すことがあり、続けて 2 回呼ぶと同じ番号になり得る。Linux・macOS・BSD は乱数の位置から探すので、
続けて同じ番号になることはまず無い。並列に走る別の試験との間でも同じことが起こり得る。

## 修正

`tests/standalone/mod.rs` の `free_port` が、このプロセスで一度返した番号を覚えておき（プロセス共有の集合）、同じ番号を
返さない。弾いた番号のソケットは従来どおり探し終わるまで持ったままにして、次の周回で同じ番号を引かないようにする。

veil 本体は変えない（設定の検査が同じ番号を拒否するのは正しい）。

## 確認

- Windows（aofish、`full-windows`）で自己完結型 E2E の 50 件が通り、プロトコル間の試験（`parity`、13 件）を 3 回続けて回して
  3 回とも通った。同じ回の VDS の 6 件の失敗は、制御プレーンのビルドに要る `PROTOC` と Python の `PYTHON3` を渡さずに回した
  環境の不足で、渡すと通った。
- Linux の自己完結型 E2E（io_uring・epoll）が通る。
