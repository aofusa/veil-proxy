# B-198: API クライアント: 上流の chunked のサイズのあふれで panic し、ヘッダ・URL・メソッドの CR/LF を検査しない

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の API・Wasm のレビュー（docs/artifacts/v0100/review-api-wasm.md） #4 #5（2026-10-26）。

## 事象

- `veil::api::client` の chunked の読み取りが、16 進のサイズと受信済みの量の加算をあふれ検査なしで行い、巨大なサイズの
  上流応答で panic（debug）・誤った長さ（release）になった。`+`・空白等を受ける `from_str_radix` も使っていた。
- メソッド・ヘッダの名前と値・既定ヘッダ・URL に CR/LF 等の制御文字があっても検査せず、要求を分割（リクエストスマグリング）できた。
- `http://unix:<path>` の URL を文字列で常に受けていたため、利用者の入力から URL を組み立てるアプリで任意の UDS へ接続できた。

## 修正

- サイズは 16 進数字だけを受け（`parse_chunk_size`）、必要量は `checked_add`（`chunk_need`）で、あふれは Protocol エラー。
- メソッドは token、ヘッダの名前は token・値は制御文字（HTAB 以外）無しを検査して Encode／Config エラー、URL は 0x20 以下と
  0x7f を Url エラーにする。
- `unix:` の URL は `ClientBuilder::unix_sockets(true)` のときだけ受ける（既定は拒否）。gRPC の `Channel` と Python の
  `GrpcChannel` は従来どおり UDS を使えるよう有効にする。

## 試験

- `src/api/client.rs` の `chunk_sizes_do_not_overflow`・`control_characters_are_rejected`・`urls_split_into_origin_and_target`（unix の可否）。

## 実施内容

- `src/api/client.rs`、`src/api/grpc/channel.rs`、`crates/veil-python/src/grpc_client.rs`。
- 文書: `docs/guide/library-kit.md`・`docs/guide/library.md`（英日）。
