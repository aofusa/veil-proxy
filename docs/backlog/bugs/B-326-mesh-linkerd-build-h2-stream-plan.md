# B-326: `mesh-linkerd` のビルドが通らない（tap の H2Service が H2StreamPlan に追従していない）

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 の警告の網羅（`docs/artifacts/v0110/warnings/sweep.sh`）の `--no-default-features --features mesh-linkerd` が
`error[E0053]: method streaming_plan has an incompatible type for trait` で失敗した。

`1ebe3cda`（B-274、v0.10.1 に入った）で `H2Service::streaming_plan` の戻り値を `Option<u64>` から `H2StreamPlan` に変えたとき、
`mesh-linkerd` でだけコンパイルされる `src/mesh/linkerd/tap.rs` の `TapH2` を直していなかった。v0.10.1 は `mesh-linkerd`
（Linkerd のプロキシのイメージ `docker/Dockerfile.mesh-linkerd` の元の `CARGO_FEATURES=mesh-istio,mesh-linkerd,...`）でビルドできない。
配布物（`full`・`full-container`）・CI は `mesh-linkerd` を含まないので気づかなかった。

## 修正

- `TapH2::streaming_plan` は `H2StreamPlan::BUFFERED` を返す（tap は本文を溜めてから処理する。従来の `None` と同じ）。

## 試験

- `cargo clippy --no-default-features --features mesh-linkerd --lib --bins -D warnings`・`--features full,mesh-istio,mesh-linkerd`。
- 再発防止: コミット前の確認に `mesh-istio,mesh-linkerd` を足す（網羅のスクリプトは各 feature の単独を回す）。
