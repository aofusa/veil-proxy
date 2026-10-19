# B-120: 単体テスト全体の並列実行が高負荷時に「thread local panicked on drop」で abort することがある

**状態: 完了（P3、テストのみ。464fa24d で試験の補助関数 `run` が `block_on` の後に `shutdown_thread` を呼ぶようにした）**

## 事象

`cargo test --lib --bins --test integration_tests --features full` を Docker のイメージビルドと並行して
（4 コアが飽和した状態で）実行したとき、`http2::upstream_mux::tests::f174_dropping_stream_sends_cancel` の
スレッドで次のメッセージを出してテストプロセス全体が abort した。

```
thread 'http2::upstream_mux::tests::f174_dropping_stream_sends_cancel' panicked at .../std/src/thread/local.rs:429:25:
fatal runtime error: thread local panicked on drop, aborting
```

## 再現状況

- 同じテストの単独実行 3 回、`cargo test --lib --features full` の全体実行 6 回（負荷なし）では再現しない。
- 該当テストと上流多重化（`src/http2/upstream_mux.rs`）は今回の作業で変更していない。

## 調査メモ

- 「thread local panicked on drop」は、スレッドローカルの破棄中に別のスレッドローカル（ランタイムの op テーブル
  等）へ触れて panic したときのもの。B-118（ワーカー終了時に残った接続タスクが破棄済みの op テーブルに触れて
  abort）と同じ型で、テストスレッドが `block_on` を抜けたあとにタスクが残っている可能性がある。
- B-112（同じ f174 系のテストが高負荷時にタイムアウトで失敗）と同じく、負荷でタイミングがずれたときだけ起きる。

## 改修案

- テスト側も本番のワーカーと同様に `block_on` の後で `runtime::shutdown_thread()` を呼び、残ったタスクを
  スレッドローカルの破棄より前に片付ける。
