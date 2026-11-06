# B-328: macOS で起動のたびにワーカーの数だけ CPU のピン留めの失敗を WARN、ライブラリ E2E が BSD の paste で落ちる

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 の PR（#10）の CI の `bindings macos-arm64`（B-327 で macos-15 に移した後に初めて最後まで走った）で 4 件が失敗した。

- `C middleware after hook`・`C client async + deferred response`・`C kit GraphQL over SSE`: 試験の `paste -sd'|'` が
  ファイルの引数を取らず、macOS（BSD）の `paste` が使い方を表示して終わる（GNU は標準入力を読む）。
- `kit log has errors`: `Failed to pin to CPU core CoreId { id: 0 }, running unpinned` の WARN。macOS（Apple silicon）は
  `thread_policy_set` のアフィニティに対応せず `core_affinity::set_for_current` が常に失敗するので、veil は macOS で
  起動のたびにワーカーの数だけ WARN を出していた（L4 のワーカーも同じ）。

## 修正

- macOS ではピン留めの失敗を `debug!` にした（ピン留め無しで動くのは仕様）。L4 のワーカーも
  `listener_workers::pin_to_core` を通す。他の OS の失敗は従来どおり WARN。
- 試験の `paste` に標準入力の `-` を明示した。
- `docs/guide/performance.md`（英日）に macOS はピン留めしないことを書いた。
