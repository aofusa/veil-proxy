# B-289: `veil_circuit_breaker_state` が状態の変わるまで出ず、Grafana の例の試験がタイミング次第で落ちる

**状態: 修正済み（v0.10.1）**

## 出典

`examples-nightly` の `examples` ジョブの `Grafana dashboard`（`tools/grafana/test-dashboard.sh`）。
2026-11-04 の 4a9b83b0・37095c9f で `FAIL Circuit breaker state (0 closed, 1 open, 2 half-open): no data`、0fd32246 では通った。

## 原因

B-278 でゲージをグループに 1 系列にし、ブレーカーの状態が変わったときだけ更新するようにした（要求ごとにラベルを
引かないため）。そのため一度も状態が変わらないグループには系列が無い。`examples/grafana` の例は存在しない上流を 1 台
含むが、ヘルスチェックがそれを外す前にブレーカーが開くかどうかはタイミング次第で、開かなければ「データなし」になる。
利用者のダッシュボードでも、正常に動いている間はパネルが空だった。

## 修正（v0.10.1）

- `UpstreamGroup::export_circuit_state`: ブレーカーのあるグループの状態ゲージを今の値で出す（ブレーカーの無いグループは出さない）。
- 起動（`entry.rs`）とリロード（`config::reload_config`）でメトリクスの実行時スイッチを設定した直後に全グループで呼ぶ
  （リロードはブレーカーを作り直すので Closed＝0）。要求ごとの経路は変えない。
- 単体試験 `metrics::tests::circuit_breaker_state_is_exported_at_load`。

## 関連

- B-278（ゲージをグループの状態にした変更）
