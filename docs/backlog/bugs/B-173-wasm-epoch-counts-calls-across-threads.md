# B-173: Proxy-Wasm のエポックが「全スレッドの呼び出し回数」で進み、複数ワーカーの負荷の下でフィルタが `interrupt` でトラップして飛ばされる

**状態: 修正済み（v0.10.0）**

## 出典

F-269（HttpArena の veil 単体の提出物）の validate で、`validate-frag.py` の 1047 の分割点のうち 1〜2 個が
`404`（`type = "Respond"` の固定応答＝フィルタが答えなかった）になった。分割の位置は毎回違い、GET でも起きる（2026-10-26）。

## 事象

- ログに `[wasm:arena] on_request_headers error: error while executing ... Caused by: wasm trap: interrupt`。トラップの位置は
  `free`・`get_property`・`proxy_on_request_headers` 等ばらばら。
- `threads = 1` では起きず、`threads = 4` で 384 本ずつの同時接続を流すと毎回 1〜2 件起きる。
- 原因: `src/wasm/registry.rs` の `config.epoch_interruption(true)` のエポックを、`src/wasm/engine.rs` は各コールバックの
  直前に `self.registry.engine().increment_epoch()` で 1 進めていた（B-56 の注記のとおり「全モジュール・全要求・全プロトコルの
  WASM 呼び出し回数」）。締切は `current_epoch + 10`。エンジンは全ワーカーで共有なので、あるワーカーの 1 回のコールバック
  （新しいインスタンスのライフサイクル、fuel の 10,000 ごとの async yield を含む）の間に **他のスレッドが 10 回** WASM を呼ぶと、
  CPU 時間に関係なく `interrupt` でトラップする。トラップしたフィルタは失敗として扱われ（`fail_closed` でなければ続行）、
  要求はフィルタ無しで進む（今回は Respond の 404、プロキシのルートならフィルタを飛ばして上流へ）。
  WAF 等のフィルタが負荷の下で飛ばされることになる。
- `FilterEngine::new` のコメント（「The tick thread increments the epoch every ~100ms, so deadline=10 allows ~1s」）は
  本来の意図（時計）を書いているが、そのスレッドは無かった。F-271 以前（v0.9.2）のコードも同じ（呼び出しのたびに 1 進める）。

## 修正（`docs/artifacts/benchmark/patches/B-173-wasm-epoch-ticker.patch`）

- `registry.rs`: `ModuleRegistry::new` でエンジンごとに専用スレッド（`veil-wasm-epoch`）を起こし、`EPOCH_TICK`（100ms）ごとに
  `increment_epoch()` する（`Engine::weak()` を持ち、エンジンが捨てられたら終わる。sleep は `server::cap_safe_sleep`）。
  1 回のコールバックの実時間の上限は約 1〜1.1 秒（`epoch_deadline` = 10）、CPU の上限は今までどおり fuel。
- `engine.rs`: 各コールバックの前の `increment_epoch()`（15 箇所）を外す（`set_epoch_deadline` は残す。B-56 の resume の前の
  引き直しも残す）。データプレーンの要求ごとの原子操作が 1 つ減る。

## 試験

- `src/wasm/engine.rs` の `exec_smoke_tests::b173_other_calls_do_not_consume_an_armed_deadline`: store を開いた後に他の要求を
  `epoch_deadline` の 3 倍実行してからコールバックを呼ぶ。修正前は `wasm trap: interrupt` で失敗し（確認済み）、修正後は通る。
- HttpArena の `validate.sh veil`（`validate-frag.py` を含む）を修正後のビルドで回し、分割の 404 が消えることを確かめる
  （`docs/artifacts/benchmark/results/v0100/`）。

## 備考

- 差分は wt/core（F-271 の `arm_store` を入れた後）の `src/wasm` に対して作った。feat/v0100 の今の `src/wasm`（F-271 の前）には
  当たらない。wt/core を統合した後に当てる。

## 実施内容（2026-10-26）

- `docs/artifacts/benchmark/patches/B-173-wasm-epoch-ticker.patch` を F-271 の後の `src/wasm` にそのまま当てた（`arm_store` を含む
  15 箇所の `increment_epoch()` を外し、`registry::spawn_epoch_ticker` を `ModuleRegistry::new` で起こす）。F-271 の文脈の使い回し
  （store を開いたまま次の要求のコールバックを呼ぶ）は、コールバックの前の `arm_store` が `set_epoch_deadline` で締切を引き直すので
  変更は要らなかった。
- 試験 `b173_other_calls_do_not_consume_an_armed_deadline` は、他の要求を 4 本のスレッドで同時に流す形にした（store を開いた後に
  合わせて `epoch_deadline` の 3 倍の呼び出し）。修正を当てる前の木では `wasm trap: interrupt` で失敗し、当てた後は通る
  （`cargo test --lib --features full wasm::` 144 件）。

