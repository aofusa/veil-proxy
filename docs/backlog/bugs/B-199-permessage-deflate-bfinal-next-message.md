# B-199: permessage-deflate: BFINAL の付いたメッセージの後、文脈を引き継ぐ接続で次のメッセージが 1007 になる

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の API・Wasm のレビュー（docs/artifacts/v0100/review-api-wasm.md） #6（2026-10-26）。

## 事象

- 伸長の入力ループが、BFINAL のブロック（`StreamEnd`）の後も同じメッセージの末尾（`00 00 ff ff`）を伸長器に与え続け、
  ストリームの終わりの後のデータとしてエラー（1007）にしていた。BFINAL を付ける送り手（一部のライブラリ）と文脈引き継ぎの
  組み合わせで、2 通目以降が切断された。

## 修正

- `StreamEnd` を受けたらそのメッセージの残りの入力を捨ててループを抜け（`break 'inputs`）、次のメッセージは伸長器を
  リセットして始める。

## 試験

- `src/api/ws/deflate.rs` の `bfinal_message_then_next_message`。

## 実施内容

- `src/api/ws/deflate.rs`。
