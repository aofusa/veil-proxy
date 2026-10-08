# B-92: `[prometheus] enabled = false`（既定）でもリクエストごとにメトリクスを記録していた

## 事象

FreeBSD aarch64 で HTTP/1.1 の CPU プロファイルを取ったところ、ログもメトリクスも無効な
計測構成なのに `logging::log_access` 経由で毎リクエスト `from_utf8`（4 回）・時刻読み取り
（2 回）・Prometheus のラベル付きカウンタ／ヒストグラム更新が走っていた。

## 原因

`metrics::record_request_metrics` は実行時スイッチ `METRICS_RUNTIME_ENABLED`（既定 `true`）が
立っていれば記録する。このスイッチは `[prometheus] enabled`（既定 `false`）に従うはずだったが、
`set_metrics_runtime_enabled` は**テストからしか呼ばれておらず**、設定ロード時・リロード時に
一度も反映されていなかった。結果として、`metrics` feature 入りのビルドは設定に関係なく
常にメトリクスを記録していた（エンドポイントは無効なので誰も読めない）。

## 修正

- 起動時（`entry.rs`）と SIGHUP リロード時（`config::reload`）に `CURRENT_CONFIG` を更新する
  直前で `set_metrics_runtime_enabled(prometheus_config.enabled)` を呼ぶ。
- `log_access` は、テキストアクセスログ（`access` ターゲットの INFO が有効）・構造化アクセスログ
  （`[access_log] enabled`）・メトリクスのいずれも無効なら、経過時間の計測や文字列変換をせずに
  即座に返す。

## 影響

`[prometheus] enabled = true` の構成は挙動不変。無効（既定）の構成はリクエストごとの
不要な処理が消える（観測可能な出力は元々無いので外部挙動の変化は無い）。
