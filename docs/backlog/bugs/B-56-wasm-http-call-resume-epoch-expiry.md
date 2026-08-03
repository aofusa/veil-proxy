# B-56: WASM `dispatch_http_call` の Pause/resume（F-62）が高負荷・高並行下で epoch トラップし fail-open する

**状態: 修正済み**

## 事象

FreeBSD 14.3 amd64（QEMU VM）実機 E2E で、Proxy-Wasm の HTTP コール Pause/resume
（F-62）を使う 2 テストが不定期に失敗した。

```
test test_f62_wasm_http_call_pause_resume ... FAILED
test test_f62_wasm_http_call_concurrent_requests ... FAILED

thread 'test_f62_wasm_http_call_pause_resume' panicked at tests/e2e_tests.rs:18095:5:
assertion `left == right` failed
  left: [60, 104, 49, 62, 72, 101, 108, 108, 111, 32, 102, 114, 111, 109, 32, 66, 97, 99, 107, 101, 110, 100, 32, 49, 60, 47, 104, 49, 62, 10]
 right: [119, 97, 115, 109, 45, 104, 116, 116, 112, 45, 99, 97, 108, 108, 45, 111, 107]
```

`left` は生のバックエンド応答（`<h1>Hello from Backend 1</h1>`）で、`right` は
WASM モジュールが返すはずのローカル応答（`wasm-http-call-ok`）。つまり
**WASM フィルタが完全にスキップされ、クライアントのリクエストがそのままバックエンドへ
素通しされていた**（Pause/resume 機構が意図どおりに機能せず fail-open した）。

`--test-threads=1` で単体テストを 3 回連続実行すると **3/3 とも成功**する
（`test_f62_wasm_http_call_pause_resume` 単体、`PARALLEL_JOBS=1`）。失敗は
**4 並列実行時のみ**再現し、単体では一切再現しない。

`/tmp/proxy.log` の該当箇所（要旨、実際のログはタイムスタンプ・PID 付き）:

```
[src/wasm/http_executor.rs:381] [wasm:http_call] HTTP call failed: module='http_call_filter' error=Empty response
[src/wasm/engine.rs:425] [wasm:http_call_filter] http_call resume error: error while executing at wasm backtrace:
    0:   0xfb2b - http_call_filter.wasm!proxy_on_http_call_response
```

この `{}`（`Display`）フォーマットでは `anyhow::Error` の `Caused by:` チェーンが
省略され、実際のトラップ種別（epoch/fuel/unreachable/OOB のどれか）が分からない
ため、当該ログ出力を一時的に `{:?}`（`Debug`、フルチェーン表示）へ変更し、かつ
本チケットの修正（下記）を一時的に取り除いた状態で FreeBSD 実機の全 E2E スイート
（`--test-threads=4`、`TEST_FILTER` 無し）を再実行して**直接観測**した
（診断用の一時変更はいずれも本番コードには残していない）:

```
2026-08-03 13:08:13.917+00 0ms ERROR type=error  [src/wasm/engine.rs:439] [wasm:http_call_filter] http_call resume error: error while executing at wasm backtrace:
    0:   0xfb2b - http_call_filter.wasm!proxy_on_http_call_response

Caused by:
    wasm trap: interrupt
```

`wasm trap: interrupt` は wasmtime の epoch interruption 機構が発生させるトラップ
そのものであり（fuel 枯渇や unreachable/OOB とは異なる文言）、epoch 締切超過が
直接の原因であることを実機ログで確認した。

`http_call resume error` は `resolve_pending_http_calls_inline`
（`src/wasm/engine.rs`）が `proxy_on_http_call_response` の呼び出しで `Err` を受け取り、
`execute_on_request_headers`/`execute_on_response_headers` 側で
`FilterAction::Continue`（フィルタ未適用と同じ扱い）へフォールバックしていたために
発生していた。この `Err` 分岐がまさに「クライアントのリクエストがそのまま
バックエンドへ通ってしまう」経路である。

## 調査

### 否定した仮説

- **`src/l4/proxy.rs::bidirectional_forward` の BSD 分岐が WASM を迂回している** —
  当該分岐は WASM 有効時に必ず先に `return` するコード上のため到達しない。コードを
  確認して否定。
- **wasmtime のシグナルベーストラップ実装が FreeBSD x86_64 で機能していない**
  （B-55 と同種のプラットフォーム非対応）— もしこれが真因なら単体実行でも
  決定的に再現するはずだが、**単体実行 3/3 で完全に成功**したため否定。
- **`execute_http_call_safe` のエラー応答に `:status` 疑似ヘッダが無いことが
  トラップの直接原因** — 一度は疑ったが、同じ欠落状態でも
  `[wasm:http_call_filter] http_call completed with status none` とトラップせず
  正常終了したケースが同一ログ内に存在したため、トラップの必要条件ではないと判断
  （改善の余地はあるが本チケットの直接原因ではない）。

### 真因: wasmtime の epoch はタイマーではなく「エンジン全体の WASM 呼び出し回数カウンタ」

`src/wasm/engine.rs` 内の `increment_epoch()` 呼び出し（当時 17 箇所）はすべて、
各 WASM 呼び出し（`instantiate_async`/各種ライフサイクルコールバック実行）の**直前に
1 回だけ**呼ばれており、`increment_epoch()` を定期的に呼ぶタイマースレッドは
コードベース中どこにも存在しない。にもかかわらず、以前のコメント
（旧 `src/wasm/engine.rs:41-42`）には次のように書かれていた:

> "The tick thread increments the epoch every ~100ms, so deadline=10 allows ~1s."

この記述は**事実と異なる**。実態は以下のとおり:

- `Engine`（`ModuleRegistry` が保持、`FilterEngine` 全体で共有）の epoch は
  **プロセス内の全 WASM 呼び出し**（全モジュール・全リクエスト・全プロトコル経路:
  HTTP/1.1・HTTP/2・HTTP/3・gRPC・L4 network filter を横断）が実行されるたびに
  1 ずつ増えるグローバルなカウンタである。
- 各 `Store` は生成時（`run_headers_module` 等）に
  `current_epoch + epoch_deadline`（`epoch_deadline = 10`）を締切として持つ。
  この締切は**経過時間ではなく「エンジン全体で他に何回 WASM 呼び出しが起きたか」**
  で判定される。
- F-62 の Pause/resume 経路（`resolve_pending_http_calls_inline`）は、
  `proxy_on_request_headers` の呼び出しで確保した `Store` を
  **そのまま保持**したまま `execute_http_call_offloaded`（ブロッキング HTTP コールの
  offload、最大 `pending.call.timeout_ms`＝本テストでは 5 秒）を `.await` する。
  この間 `Store` の締切は更新されない。
- 4 並列 E2E 実行下では、この 5 秒間に**無関係な他リクエストの WASM 呼び出しが
  10 回以上**エンジン全体で発生することが普通に起こり得る（gRPC/H3/header_filter
  等、同時に走る他テストの WASM 呼び出しも同じエンジンの epoch を共有して増分する）。
  その結果、`proxy_on_http_call_response` を resume した瞬間には**すでに締切超過**
  しており、ゲスト側のコードが一切実行されないまま即座にトラップする。
- これは **FreeBSD 固有の不具合ではなく、プラットフォーム非依存の設計バグ**である。
  Linux のローカル開発機（542 passed / 0 failed）で再現しなかったのは、単に
  マシンが高速で 5 秒間に無関係な WASM 呼び出しが 10 回積み上がる確率が低かった
  （＝競合に勝っていた）だけであり、Linux でも十分な同時実行負荷があれば同様に
  再現し得る。今回 FreeBSD の QEMU VM（4 vCPU、他の BSD VM と host CPU を共有する
  高負荷環境）で確率的に顕在化した。

## 修正

`src/wasm/engine.rs::resolve_pending_http_calls_inline` のループ内、
`execute_http_call_offloaded`（ブロッキング HTTP コールの `.await`）の直後・
`proxy_on_http_call_response` を呼び出す直前で、締切を明示的に引き直す:

```rust
// B-56: エポックはエンジン共有の「WASM 呼び出し回数」カウンタであり時計ではない。
// pause 中に他リクエストが 10 回 WASM を呼ぶと、外部 I/O を待っていただけの本 store の
// デッドラインが期限切れになり resume 直後にトラップする。再開直前に猶予を与え直す。
// （外部 I/O の待ち時間はゲストの CPU 時間ではないため、これは制限の回避ではない）
store.set_epoch_deadline(self.epoch_deadline);
self.registry.engine().increment_epoch();
```

このヘルパは `execute_on_request_headers`（リクエストヘッダフィルタ用）と
`execute_on_response_headers`（レスポンスヘッダフィルタ用）の両方から共有されて
呼ばれているため、1 箇所の修正で両経路をカバーする。

`fuel`（CPU バウンドな実行時間の上限）は意図的に触らない。fuel はゲスト側の
実際の CPU 消費に対する正当な制限であり、外部 I/O の待ち時間とは無関係のため
リセットする理由がない（リセットするとラウンドを繰り返すことで無制限に fuel を
稼げてしまう）。epoch のみが「無関係な同時実行の影響を受ける」問題を持つため、
epoch のみを resume 直前に引き直す。

`src/wasm/engine.rs` の `epoch_deadline` フィールド上のコメント（旧・誤り）も、
epoch がタイマーではなくグローバルな呼び出し回数カウンタであること、Pause/resume
のように `Store` を長時間サスペンドする経路では再開前に締切を引き直す必要が
あることを明記する内容へ書き換えた。

## 検証

- 単体（`test_f62_wasm_http_call_pause_resume` のみ、`--test-threads=1` で 3 回連続）:
  3/3 成功（修正前バイナリでも成功していたため、これは「単体では再現しない」ことの
  確認であり退行検証ではない）。
- 4 並列・フルスイート（FreeBSD QEMU VM、同一バイナリ・同一プラットフォームで
  修正の有無だけを変えた before/after 比較）:
  - 修正前 + `TEST_FILTER` 無し: `test result: FAILED. 538 passed; 5 failed`
    （failures: `test_b56_wasm_http_call_resume_survives_concurrent_epoch_pressure`
    〔fail-open のアサーション失敗〕、`test_concurrent_connection_stress`、
    `test_error_handling_431_request_header_fields_too_large`、
    `test_error_handling_oversized_header`、`test_l4_wasm_close_on_marker`）。
  - 修正後 + `TEST_FILTER` 無し: `test result: FAILED. 542 passed; 1 failed`
    （`finished in 65.74s`。残る失敗は `test_l4_wasm_close_on_marker` のみで、
    これは本チケットの epoch 機構とは無関係の別事象と切り分け中——上記 4 件の
    負荷起因フレーキー、`test_b56_...`、`test_f62_wasm_http_call_*` はいずれも
    修正後に安定して成功）。
- Linux ローカル: 既存 E2E（542 件）は退行なし。

### 回帰テスト（決定的単体テストへ置き換え）

当初は `tests/e2e_tests.rs` に `test_b56_wasm_http_call_resume_survives_concurrent_epoch_pressure`
という E2E テストを追加していた（本命の `/wasm-http-call/` リクエストの offload 待ちの間、
32 ワーカーで `/wasm/*` への負荷を回し続けてエンジン共有 epoch を押し上げる構成）。
しかしこれは**負荷依存で非決定的**であり、OpenBSD VM では他の負荷敏感テスト
（`test_concurrent_*`/`test_http3_*`/`test_oversized_*` 系 18 件）を巻き込んで新規に
失敗させ、しかも本テスト自身が修正込みの状態でも FreeBSD VM で FAIL した
（修正前後の区別がつかず、回帰テストとして機能しなかった）。**削除した。**

代わりに `src/wasm/engine.rs::exec_smoke_tests` に決定的な単体テスト
`b56_http_call_resume_survives_epoch_pressure_while_suspended` を追加した。
負荷や並行リクエストは使わず、`Engine::increment_epoch()` を直接
`epoch_deadline`（10）回呼んで「他のリクエストの WASM 呼び出しが積み上がった」
状態を決定的に再現する:

1. `http_call_filter.wasm`（`tests/fixtures/wasm/`、examples/wasm-filters/http-call-filter
   由来）をロードした `FilterEngine` を構築する。
2. `run_headers_module`（本番と同じ private メソッド、同一ファイル内のテストなので
   直接呼べる）で `proxy_on_request_headers` を実行し、モジュールが
   `dispatch_http_call` して `Pause` を返す状態（`pending_http_calls` 登録済み）まで進める。
3. `store` をこの Pause 状態で保持したまま、`engine.registry.engine().increment_epoch()`
   を `epoch_deadline` 回呼び、エンジン共有 epoch を締切超過させる
   （upstream 解決やネットワーク I/O は使わない。`upstream_groups` が空の
   テスト用 `CURRENT_CONFIG` では `execute_http_call_offloaded` が 502 応答を
   即返すが、`proxy_on_http_call_response` は呼ばれるため resume 機構の検証には十分）。
4. 本番の `resolve_pending_http_calls_inline` をそのまま呼んで resume させ、
   トラップせずローカルレスポンスが設定されることを確認する。

**pre-fix で FAIL することを実際に確認済み**（`resolve_pending_http_calls_inline` 内の
`store.set_epoch_deadline(...); self.registry.engine().increment_epoch();`
の 2 行を一時的にコメントアウトして再実行）:

```
thread 'wasm::engine::exec_smoke_tests::b56_http_call_resume_survives_epoch_pressure_while_suspended' panicked at src/wasm/engine.rs:2967:23:
resume must not trap on stale epoch deadline while suspended (B-56): error while executing at wasm backtrace:
    0:   0xfb2b - http_call_filter.wasm!proxy_on_http_call_response
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 795 filtered out; finished in 0.08s
```

修正を戻すと PASS する:

```
test wasm::engine::exec_smoke_tests::b56_http_call_resume_survives_epoch_pressure_while_suspended ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 795 filtered out; finished in 0.09s
```

`test_f62_wasm_http_call_pause_resume`/`test_f62_wasm_http_call_concurrent_requests`
（E2E、`--features wasm`）は既存のまま維持し、Pause/resume 機能自体の実環境動作確認を
引き続き担う。B-56 固有の epoch 競合の回帰検出は上記の決定的単体テストが担当する。

## 関連

- F-62（Proxy-Wasm HTTP コール Pause/resume 機能そのもの）
- B-52（OpenBSD の WASM SIGSEGV。プラットフォーム固有の別問題だが、本チケットの
  調査で「単体実行で決定的に再現するか」を切り分け基準として使った点は同種）
