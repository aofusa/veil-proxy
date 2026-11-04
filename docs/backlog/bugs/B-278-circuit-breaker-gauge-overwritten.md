# B-278: `veil_circuit_breaker_state` が最後に記録したサーバの状態で上書きされ、`veil_outlier_ejected` が 0 に戻らない

**状態: 修正済み（v0.10.1）**

## 出典

[B-272](B-272-circuit-breaker-outlier-h1-only.md)・[B-273](B-273-metrics-recorded-on-some-protocols-only.md) の統合試験で、ブレーカーが開いたのに
ゲージが 0 のままになることがあって見つけた（実機で確認）。

## 事象

- `veil_circuit_breaker_state{upstream=...}` はグループ（upstream）ごとの系列なのに、要求の結果を記録するたびに**そのサーバ**の状態で
  書き込んでいた。2 台のうち 1 台が Open でも、もう 1 台（Closed）の要求を記録した時点で 0 に戻る。
- `veil_outlier_ejected` は排除したときに 1 にするだけで、排除が終わっても 0 に戻らなかった。

## 修正（v0.10.1）

- `UpstreamGroup::record_request` で結果を記録する前後の（状態・開いた回数・排除中か）を比べ、変わったときだけメトリクスを更新する。
  ゲージの値はグループの集約（どれかが Open なら 1、なければどれかが HalfOpen なら 2、それ以外は 0）。`veil_circuit_breaker_open_total` は
  開いた回数が増えた分だけ足す。排除が終わったら `veil_outlier_ejected` を 0 にする（`src/config/upstream_group.rs`）。
- 要求ごとの処理は前後の比較だけ（状態が変わらなければメトリクスに触れない）。
- 統合試験 `b272_circuit_breaker_opens_on_every_protocol`（失敗するサーバと正常なサーバの 2 台で、ゲージ 1・開いた回数 1）。
- `docs/guide/observability.md`・`load-balancing.md`（英日）に集約の意味を書いた。

## 関連

- [B-272](B-272-circuit-breaker-outlier-h1-only.md)、[B-273](B-273-metrics-recorded-on-some-protocols-only.md)
