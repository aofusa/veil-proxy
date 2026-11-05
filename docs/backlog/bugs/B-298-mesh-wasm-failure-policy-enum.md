# B-298: メッシュの Wasm の `failure_policy` の番号を読み違え、FAIL_RELOAD が fail open、FAIL_OPEN が fail closed になる

**状態: 修正済み（2026-11-05、v0.11.0）**

## 事象

- Envoy の `envoy.extensions.wasm.v3.FailurePolicy` は UNSPECIFIED=0・FAIL_RELOAD=1・FAIL_CLOSED=2・FAIL_OPEN=3（envoy-types 0.7.7 の
  `envoy.extensions.wasm.v3.rs:297` で確認）。
- veil は `let fail_open = pc.fail_open || pc.failure_policy == 1;`（`src/mesh/xds/translate.rs:151`）で、`src/mesh/xds/proto.rs:1355` のコメントも
  「0 = FAIL_CLOSED, 1 = FAIL_OPEN, 2 = FAIL_RELOAD」と順番が違う。既定（0）と FAIL_CLOSED（2）だけが正しく動く。

## 影響

- `failure_policy: FAIL_OPEN` を明示したプラグインが失敗すると 503（可用性）、FAIL_RELOAD を明示したものが素通し（安全性）。Istio が `fail_open`
  （非推奨の bool）と `failure_policy` のどちらを送るかは未確認（メッシュの E2E で確かめる）。

## 改修案

- 対応を純粋な関数へ出し、明示の `failure_policy` を非推奨の `fail_open` より優先する（[F-293](../features/F-293-wasm-failure-policy.md) の対応表）。
  `proto.rs` のコメントを直す。4 × 2 の全組み合わせの単体試験。

## 修正

- `FailurePolicy::from_envoy`（UNSPECIFIED は `fail_open` に従う・1=FAIL_RELOAD・2=FAIL_CLOSED・3=FAIL_OPEN）を `xds/translate.rs` で使い、読み込めないモジュールを飛ばすのは fail open のときだけにした。`proto.rs` のコメントも直した。8 通りの対応を単体試験にした。

## 関連

- [F-293](../features/F-293-wasm-failure-policy.md)
