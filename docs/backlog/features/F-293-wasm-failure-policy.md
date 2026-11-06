# F-293: Proxy-Wasm のフィルタが失敗したときの扱い（`failure_policy`）をメッシュ外でも選べるようにし、既定を fail closed にする

**状態: 完了（2026-11-05、v0.11.0）**

調査の全文（失敗点 × プロトコルの表）は `docs/artifacts/v0110/review-05-wasm-fail-open.md`（git 管理外）。行番号は f8dc80de 時点。

## 今の状態

- メッシュ外（`[wasm]`・`[[wasm.modules]]`）では、コールバックの trap・fuel 切れ・epoch の時間切れでエラーをログに出し、要求を続ける（fail open）。
  `docs/guide/wasm.md:232-235`（ja :225）・`docs/releases/v0.10.1.md` の Known limitations に書いてある。メッシュの `WasmPlugin` の
  `FAIL_CLOSED` だけが 503（`FilterEngine.fail_closed`、`src/wasm/engine.rs:163-171`）。WAF のようなセキュリティのフィルタは、負荷で時間切れになると素通しになる
  （[B-110](../bugs/B-110-netbsd-x86_64-wasm-waf-load-flaky.md) は NetBSD でこれを踏んだ）。
- fail closed が設定されていても素通しになる経路がある（[B-299](../bugs/B-299-wasm-fail-closed-bypasses.md)）。Pause は待たれない（[B-300](../bugs/B-300-wasm-pause-and-response-local-reply-ignored.md)）。
  メッシュの enum の読み違い（[B-298](../bugs/B-298-mesh-wasm-failure-policy-enum.md)）。起動時のコールバックの結果を見ていない（[B-301](../bugs/B-301-wasm-lifecycle-results-ignored.md)）。

## 改修内容（設計）

- 設定: `failure_policy = "fail_closed" | "fail_open" | "fail_reload"` を `[wasm.defaults]` に置き、`[[wasm.modules]]` で上書きできる。ロード時に
  `LoadedModule` へ解決し、メッシュ（ECDS）も同じフィールドに入れる（`FilterEngine.fail_closed` の bool はやめる）。
- **既定は `fail_closed`**（Envoy の既定。素通しを防ぐ。2026-11-05 に利用者が決定）。互換性の変更なので、リリースノートの Upgrade notes に「`failure_policy = "fail_open"` で
  以前の動き」と書く。`tools/container_security` の W-06（`fuel-burner-filter`）の期待値も直す。
- Envoy の値との対応: FAIL_OPEN、または UNSPECIFIED で `fail_open = true` → `fail_open`。FAIL_CLOSED、または UNSPECIFIED で `fail_open = false` → `fail_closed`。
  FAIL_RELOAD → `fail_reload`（今の要求は 503、インスタンスを捨てて作り直す）。Envoy の FAIL_CLOSED は VM を壊れたままにして設定が変わるまで全要求を
  503 にするが、veil はインスタンスを作り直すので、**1 つの要求の失敗を後の要求へ持ち越さない**（1 つの悪い要求で全体を落とさない）。
- 失敗したときの動き:
  - 要求の段・応答ヘッダを送る前の段: 503（gRPC は trailers-only の `grpc-status: 14`）。
  - ストリーミングで応答を送り始めた後: ストリームのリセット（HTTP/1.1 は接続を閉じる）。
  - 守れない Pause も失敗として数える（B-300）。
  - L4 のネットワークフィルタ: 接続を閉じる。
  - 失敗の前に設定されたローカルレスポンスはポリシーに関係なく送る（B-299）。`on_log` は呼ぶ。
- ホットパス: 成功時は変わらない。既存の `Err` の腕で 1 バイトの enum を読むだけ。503 は空の `Vec` で作り確保しない。
- メトリクス: `veil_wasm_failures_total{filter, phase, reason, action}`（reason は trap・fuel・timeout（`Trap::OutOfFuel`・`Trap::Interrupt`）・instantiate・
  lifecycle・http_call_resume・unsupported_pause、action は continued・rejected）。ログは `log_throttled!`（今は要求ごとに `error!`）。
- インスタンス枠（`total_memories`、既定 128）が尽きたとき fail closed だと 503 の DoS になるので、上限付きで空きを待つ（B-299）。
- 文書（同じ PR）: `wasm.md`（英日）の Default settings、`mesh.md:78`、`examples/config.toml`、Known limitations の行を消す。あわせて直す食い違い:
  - fail open の説明「フィルタが無かったものとして続く」→ trap の前に加えたヘッダの変更は残る（Envoy も残す。コードではなく文書を直す）。
  - `wasm.md:14`（英日）「約 1 万命令ごとに譲る」→ 今は約 1ms（1M fuel）。
  - `max_execution_time_ms` は「1 回の Wasm の呼び出しごと」と書いているが、再利用しないモジュールでは起動時のコールバックと要求のコールバックが同じ予算を使う。
  - Istio のリソースの値は `failStrategy: FAIL_CLOSE`（Envoy の名前 `FAIL_CLOSED` と違う）。
  - `allowed_upstreams` が空なら全部許可、を書いていない。
  - 応答ヘッダの段のローカルレスポンスが無視されることを段の表に書いていない（B-300 で直すなら不要）。

## 受け入れ条件

- 単体: インラインの WAT（wasmtime の `wat` feature は有効）で小さいモジュールを作る（`tests/` のフィクスチャに依存しない）。各ポリシーで trap・fuel 切れ、
  ローカルレスポンスの後の trap、`on_configure` が false、http_call の resume の trap、応答・本文の段の trap、`total_memories = 1` の枠切れ、呼び出しの無い Pause、
  設定の serde、メッシュの 8 通りの対応。
- E2E（`tests/e2e_tests/wasm.rs`）: `trap-filter`・`fuel-burner-filter` のルートを open・closed で、503 と 200、キープアライブが続く、新しいメトリクス、アクセスログ。
  HTTP/1.1・HTTP/2・HTTP/3、io_uring・epoll。`test_wasm_timeout`・`test_wasm_error_handling` が実際に失敗を起こしていないので直す。
- メッシュ: `tools/k8s/mesh-e2e.sh veil wasm` を各 failStrategy で回し、Istio が送る ECDS の `failure_policy` の値を記録する。

## 修正

- `[wasm.defaults] failure_policy`（既定 `fail_closed`）と `[[wasm.modules]] failure_policy` を追加し、ロード時に `LoadedModule.failure_policy` へ解決（`FilterEngine.fail_closed` の bool は削除。メッシュも同じフィールド）。
- `src/wasm/engine.rs`: `settle`（Pause の解決・トラップ前のローカルレスポンス）と `record_failure`（`veil_wasm_failures_total{filter,phase,reason,action}`・`log_throttled!`・`fail_reload` でプールの世代を進める）を全フェーズ（要求・応答ヘッダ、本文、トレーラー）で共有。成功時の経路は変わらない。
- 応答ヘッダの段のローカルレスポンス・拒否を HTTP/1.1・HTTP/2・HTTP/3・メッシュの全経路で元の応答の代わりに送る（B-300）。
- 文書: `wasm.md`（英日）に「失敗の扱い」、`observability.md`・`mesh.md`・`examples/config.toml`。`yield` の説明（約 1ms）・`max_execution_time_ms`・`allowed_upstreams` の食い違いも直した。
- 試験: `src/wasm/tests.rs` の `failure_policy_tests`（インラインの WAT。trap・fuel・トラップ前のローカルレスポンス・on_configure の false・Pause・本文と応答ヘッダの段・枠切れ・serde・Envoy の 8 通り）、E2E `test_wasm_timeout`・`test_wasm_error_handling`（HTTP/1.1・HTTP/2・HTTP/3、メトリクス）、container_security の W-06 は 503 を期待する。

## 関連

- [B-298](../bugs/B-298-mesh-wasm-failure-policy-enum.md)〜[B-301](../bugs/B-301-wasm-lifecycle-results-ignored.md)、[B-56](../bugs/B-56-wasm-http-call-resume-epoch-expiry.md)、
  [B-110](../bugs/B-110-netbsd-x86_64-wasm-waf-load-flaky.md)、[F-285](F-285-wasm-response-body-h1-h2.md)
