# B-195: ストリーミング受信の multipart に本文全体の上限が無い

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の API・Wasm のレビュー（docs/artifacts/v0100/review-api-wasm.md） #1（2026-10-26）。

## 事象

- ストリーミング受信の multipart（Rust の `Multipart`、Python・C の Form/File ルート）は、1 部分（`multipart_max_file_size`）・
  部分の数・ヘッダの大きさの上限はあるが、本文全体の上限が無かった。小さな部分を上限の数まで並べるか、上限ぎりぎりの
  ファイル部分を多数送ると、ハンドラ（保存先のディスク等）に際限なくデータを渡せた。

## 修正

- 設定 `[api] multipart_max_total_size`（既定 0 = `max_body_size` と `multipart_max_file_size` の大きい方）を足し、
  `Core::feed` で受けた本文の総量を数える（加算は `saturating_add`）。超えたら次の部分・断片を取り出すときに 413
  `multipart body is too large` を返す。要求ごとの確保は増やしていない（カウンタ 1 個）。

## 試験

- `src/api/multipart.rs` の `total_size_limit_gives_413`（上限ちょうどは通り、超えると 413）。

## 実施内容

- `src/api/multipart.rs`（`Limits::max_total_size`・`Core::total`・`check_total`）、`src/api/app.rs`（`ApiSettings`）。
- 文書: `docs/guide/library.md`（英日）、`crates/veil-python/docs/requests-responses.md`、`crates/veil-capi/README.md`、
  `examples/config.toml`。
