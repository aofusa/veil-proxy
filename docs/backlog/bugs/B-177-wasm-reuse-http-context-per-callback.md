# B-177: Proxy-Wasm の `instance_reuse` で http context が要求ごとでなくコールバックごとに作られ、フェーズをまたぐ状態が消える

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のレビュー（`docs/artifacts/v0100/review-api-wasm.md` の #8、2026-10-26）。

## 事象

- 文書（`docs/guide/wasm.md` の「Reusing the VM Context」）は「各要求は http context を 1 つ作り、要求が終わったら
  `proxy_on_delete`。Envoy と同じ寿命」と書くが、実装（F-271）は要求ヘッダ・要求本文・応答ヘッダ・応答本文・`on_log` の
  フェーズごとに `open_session` → `proxy_on_context_create` → コールバック → `proxy_on_delete` をしていた。フェーズごとに
  新しい context ID（場合によっては別のインスタンス）になる。
- `on_http_request_headers` で http context に保存した値を `on_http_response_headers`・`on_log` で使うフィルタ（Envoy では
  普通の書き方）は値を失う。代わりに root・グローバルに置くと、同じインスタンスで並行する別の要求のフェーズが割り込み、
  他人の要求の値を読む。
- プールの出し入れが std の `Mutex`（スレッドで区画を分けて競合は小さいが、ホットパス規則の「同期ロック待ち」に当たる）。

## 修正

- 要求ごとの状態 `RequestSessions`（`src/wasm/instance_pool.rs`）を作り、要求のフェーズに渡すモジュールの一覧
  `WasmModules`（`src/wasm_plugin_config.rs`。モジュールの `Arc<Vec<ModuleRef>>` と要求ごとの状態）に持たせる。HTTP/1.1・
  HTTP/2・HTTP/3 で要求の最初に `WasmModules::for_request` で作り、全フェーズと `on_log` に渡す。モジュールの無いルートの
  要求は何も確保しない（モジュールのあるルートの要求は `Rc` を 1 つと、`instance_reuse` のモジュールの context を記録する
  `Vec` を 1 つ確保する）。
- `instance_reuse` のモジュールは、要求の最初のフェーズでインスタンス（空いているもの、無ければ新しく作る）に http context を
  作り、以後のフェーズは同じインスタンスの同じ context ID で呼ぶ。`on_log` の後に `proxy_on_delete` する。1 つのインスタンスに
  複数の要求の context が住む（Envoy の 1 ワーカー 1 VM と同じ）。
- コールバックの間はインスタンスを取り出して使い、同じインスタンスに context を持つ別の要求がその間にフェーズを呼んだら、
  戻されるまで待つ（`InstanceSlot::acquire`。ワーカーのスレッドの中の `RefCell` と `Waker` で、ロックは使わない）。プールは
  ワーカーのスレッドごとの持ち分にした（スレッドローカル。`Mutex` を外した）。
- 要求が `on_log` まで行かずに終わったら（切断・エラー）、`RequestSessions` の `Drop` が context ID をインスタンスの
  `pending_delete` に積み、次にそのインスタンスを使うときに `proxy_on_delete` する（context がインスタンスの中に溜まらない）。
- トラップ・エラーのインスタンスは今までどおり捨てる。そこに context を持っていた（別の）要求の以後のフェーズは、新しい
  context（空いているインスタンス、無ければ新しく作る）で続ける。フィルタを飛ばさない（その context に持っていた状態は失われる。
  B-177 の前と同じ）。
- 要求ごとの状態の無い呼び出し（メッシュの Istio の WasmPlugin・トレイラーのフェーズ・L4）は今までどおりコールバックごとに
  context を作って消す。使い回さないモジュール（`instance_reuse = false`）も今までどおり。

## 残る制約

- `dispatch_http_call` の応答はコールバックの中で（`resolve_pending_http_calls_inline`、上流への呼び出しは offload）待つので、
  その間インスタンスを取り出したままにする。同じインスタンスに context を持つ別の要求の次のフェーズはその間待つ（新しい要求は
  空いている別のインスタンスを使う）。Envoy は Pause の間も VM が他の context を処理する。HTTP 呼び出しを使うフィルタで
  `instance_reuse` を使うときの待ちを減らすには、Pause の間インスタンスを戻す形にする必要がある（別チケット）。
- コールバックの途中で要求が捨てられたら（クライアントの切断で future が drop された等）、ゲストの状態が途中のままなので
  インスタンスごと捨てる。そこに context を持つ他の要求の以後のフェーズは新しい context で続く（トラップと同じ）。

## 試験

- `src/wasm/tests.rs` の `instance_reuse_tests`:
  - `reused_instance_keeps_one_http_context_per_request`: 要求ヘッダ → 要求本文 → `on_log` が同じ context（フィルタの
    `x-instance-seq` が増えない）、`on_log` の後に要求ごとの状態が空になる、2 つの要求が 1 つのインスタンスに別々の context を
    持つ、要求ごとの状態が無い呼び出しは従来どおり。
  - `dropped_request_context_is_deleted_on_next_use`: `on_log` 前に要求を捨てたら、次の利用で `proxy_on_delete` する。
  - `trap_of_another_request_moves_later_phases_to_a_new_context`: 別の要求のトラップでインスタンスを捨てたら、その要求の
    以後のフェーズは新しいインスタンスの新しい context でフィルタを通る。
- `src/wasm/instance_pool.rs` の単体テスト（捨てた要求の context の積み方、外した context は積まない）。
- HttpArena の veil の提出物の validate（要求本文・`instance_reuse` のルートを HTTP/1.1・HTTP/2・HTTP/3 で通る）。

## 実施内容（2026-10-26）

- 上の修正。`docs/guide/wasm.md`（英日）の「Reusing the VM Context」に、フェーズをまたいで同じ context であること・
  1 つのインスタンスに複数の要求の context が住むこと・`on_log` まで行かなかった要求の context の消し方を書いた。
- 計測は B-175 の表の B-175 の行がこの変更を含まない版（`veil-v1`）。この変更を含めた版の数字は B-175 の実施内容の追記を見る。
