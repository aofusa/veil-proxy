# B-336: 起動時の Proxy-Wasm の検査が、冷えたページキャッシュと遅いディスクで `interrupt` になって起動できない

**状態: 修正済み（v0.12.0）**

## 出典

F-297 の reactor の変更の後に回した epoll の E2E（`VEIL_E2E_FEATURES="full,epoll" ./tests/e2e_setup.sh test`）が、テストを 1 件も
実行せずに `exit=1` で止まった（2026-11-06）。reactor の変更とは無関係で、`/tmp/proxy.log` の末尾は次のとおり。

```
Config load error: Failed to initialize WASM Filter Engine: wasm module 'grpc_trailer_filter' failed to start: error while executing at wasm backtrace:
    0:   0xb673 - grpc_trailer_filter.wasm!proxy_on_context_create: wasm trap: interrupt
```

## 事象

- プロキシは設定の読み込みで終了し、E2E の台本は「WASM AOT compile?」として 180 秒待ってから失敗を報告した。
- 同じバイナリ・同じ設定で 3 回起動し直すと、すべて正常に起動した（間欠的）。
- 失敗した回の `.cwasm` の mtime は数日前のまま＝コンパイルではなくキャッシュヒット。それでも `waf_filter` の読み込みは 450ms、
  `fuel_burner` は 490ms（普段は 4ms）。直前まで 4 分のビルドが走っていて、ページキャッシュが冷えていた。ディスクは 5400rpm の HDD。

## 原因

`FilterEngine::validate` は各モジュールのライフサイクルを 1 回のコールバックと同じ締切（`epoch_deadline` = 10 ティック、約 1 秒）で
走らせる。B-173 以降エポックは実時間で進む。`Module::deserialize_file` の機械語は mmap なので、最初の実行はページフォールトで
ディスクを待ち、ゲストが CPU を使っていなくても締切が過ぎる。

**確認できた範囲:** `.cwasm` をページキャッシュから追い出して、同期書き込みとランダム読み取りでディスクを混ませると、8 モジュールの
検査は普段の 30ms から 1〜2.5 秒に延びた（3 モジュールの区間で最大 1.9 秒）。ただしこの負荷では 14 回試してトラップそのものは
再現していない（1 モジュールが 1 秒を超えるには足りない）。失敗した回の時刻は stderr に残らないので、上の原因は状況と機構からの
推定で、トラップの再現による確認ではない。機構そのものは、Store を用意してからエポックを進める単体テストで決定的に確かめた。

## 修正

- `FilterEngine::validation_store`: 起動時の検査の Store の実時間の締切だけを `VALIDATE_EPOCH_DEADLINE`（600 ティック、約 60 秒）にする。
  CPU の上限は同じ燃料なので、暴走するモジュールは今までどおり拒否する。検査はデータプレーンの外で 1 度だけ走る。
- 要求のコールバックの締切は変えない。

## 試験

- `wasm::engine::tests::b336_validation_survives_a_stall_longer_than_the_callback_deadline`（`validation_store` の後にエポックを
  `epoch_deadline` の 2 倍進めても検査がトラップしない）。
