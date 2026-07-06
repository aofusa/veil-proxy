# F-87: io_uring Future のランダム Drop（キャンセル安全性）テスト

出典: [container_security_review_report.md](../../artifacts/container_security_review_report.md) 提案4。関連: [B-07](../bugs/B-07-iouring-future-drop-uaf.md)（Future Drop UAF）・[F-46](F-46-typed-task-pool-optable-slab.md)（世代スラブ）。

## 目的

io_uring はカーネルがユーザー空間バッファへ非同期に書き込むため、`.await` 中の Future が
いつでも Drop されうる（`timeout` / `select` / 接続切断）Rust の非同期環境では、Drop 後に
遅延到着した CQE が解放済みバッファへ書き込まないこと（detach ガード + 世代スラブの健全性）が
クリティカルである。I/O Future をランダムなタイミングで強制 Drop するプロパティテストを追加し、
Ownership / Pin のライフタイム不具合を機械的に検出する。

## 実装済み（2026-07-06）

- 統合テスト `tests/runtime_cancellation_test.rs` を新設（実リング。io_uring 不可環境は
  スキップ）:
  - `random_drop_of_recv_send_futures_keeps_runtime_alive`: 接続ごとに送信遅延をばらつかせた
    ペイロードサーバーに対し、recv/send Future を (a) 提出前 Drop、(b)(c) `timeout` の
    ランダム µs 打ち切りによる in-flight Drop、(d) 同一接続での多重キャンセル、の 4 パターンで
    120 回破棄。8 回ごとに新規接続の liveness プローブでペイロード完全受信・内容一致を assert。
  - `random_drop_of_accept_and_timer_futures_keeps_listener_alive`: in-flight ACCEPT / TIMEOUT
    Future のランダム Drop 後もリスナーが accept を継続できること（B-07 の「孤立 accept が
    接続を奪う」対策 = detach + ASYNC_CANCEL の実カーネル検証。キャンセル競合はリトライで許容）。
- 乱数は決定的 LCG。`VEIL_CANCEL_TEST_SEED` で上書き可能、使用シードを stderr に出力。
- シード 5 種（1/42/999/…）で安定通過を確認。
- OpTable 単体レベルのランダム操作列は F-84 の fuzz ターゲットが担う。

## 改修内容（当初案）

- 実リングを使う統合テスト `tests/runtime_cancellation_test.rs` を新設:
  - loopback TCP（listener + client）上で `RecvFuture`/`SendFuture`/`AcceptFuture`/
    `TimeoutFuture` 等を生成し、**ランダムな poll 回数の後に Drop** する
    （0 回 poll・提出直後・完了直前の各タイミングを網羅）。
  - Drop 後もイベントループを回して遅延 CQE を全量処理し、panic・UAF（ASAN 併用時）・
    op スロットリーク（全 CQE 処理後に in-flight op が残らない）がないことを検証する。
  - 乱数シードは環境変数で固定可能にし、失敗再現をログへ出力する。
- OpTable 単体レベルのランダム操作列プロパティは F-84 の fuzz ターゲットが担う
  （本チケットは実リング + 実カーネル遅延 CQE の結合検証）。
- F-85 の sanitizer ビルドと組み合わせて UAF を実行レベルで検出できるようにする。

## 受け入れ条件

- [x] ランダム Drop テストが `cargo test` で安定して通過すること（io_uring 使用可能ホスト。
      シード 5 種で確認）。
- [x] 発見された不具合はバグチケット化し修正すること（本実装では新規不具合の検出なし。
      B-07/B-24 の既存対策が実カーネルで有効であることを確認）。

## 依存・リスク

- io_uring 非対応環境（CI コンテナ等）ではリング生成失敗時にスキップする分岐が必要。
- タイミング依存のフレーキー化を避けるため、遅延 CQE 待ちは poll 回数上限つきで行う。
