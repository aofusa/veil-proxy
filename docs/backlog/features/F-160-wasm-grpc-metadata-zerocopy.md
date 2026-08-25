# F-160: WASM gRPC メタデータ／メッセージのゼロコピー化

**優先度**: P2
**ステータス**: 完了（構造的なアロケーション削減。既存の負荷ハーネスでは経路を踏まないため rps 計測なし）
**関連**: F-134（`proxy_grpc_call` 系の実行ループ）、F-139（ノンブロッキング化・接続プール）

---

## 背景

`proxy_grpc_call` / `proxy_grpc_stream` / `proxy_grpc_send` のホスト関数は
**データプレーンのワーカースレッド上**（WASM フィルタ実行中）で走る＝ホットパスである。
そこで次のアロケーションが発生していた。

1. `deserialize_grpc_metadata(&bytes) -> Vec<(String, String)>` — メタデータ 1 ペアにつき
   `String` 2 個 + `Vec` 1 個。ゲストが渡した直列化バイト列をわざわざ所有文字列へ展開していた。
2. `message.clone()` — **リクエストメッセージ本体のディープコピー**（`HttpContext` と
   グローバルレジストリの両方へ入れるためだけに全体を複製）。
3. tick スレッド側でも `String::from_utf8_lossy(..).to_string()` → `k.clone().into_bytes()` と
   同じ内容を 2 回作り直していた。

## 改修内容

- **メタデータは直列化バイト列のまま持ち回る**: `GrpcMetadataBlob`（`bytes::Bytes` の newtype）と
  借用イテレータ `GrpcMetadataIter`（`(&[u8], &[u8])` を返す）を追加し、HPACK エンコーダへ
  そのまま渡す。ゲスト由来の不正・途中切れ入力では panic せず走査を打ち切る（従来と同じ挙動）。
- **メッセージは `Bytes` で共有**: `HttpContext::pending_grpc_calls` の値と
  `PendingGrpcUnaryCall::messages` を `Bytes` にし、二重登録の `clone()` を参照カウント +1 にした。
- **tick スレッド側の型を `(Bytes, Bytes)` に統一**: `GrpcUnaryResult` / `GrpcCallResponse` /
  `FilterEngine::on_grpc_receive_*` のメタデータ表現を変更し、HPACK デコード結果の
  `String` 再変換を排除した。

`HttpContext` のヘッダマップ型（`Vec<(Vec<u8>, Vec<u8>)>`）は変更していない。
`src/wasm/host/headers.rs` の `get_headers` が全マップ共通でこの型を返しており、変更すると
15 ファイル・100 箇所に波及するため。境界の変換は従来と同じ 1 回に留まる。

## 計測について

**既存の負荷ハーネス（`tools/perf`）ではこの経路を踏めない**。同梱の WASM モジュールは
`passthrough_filter.wasm`（ヘッダ操作のみ）で、`proxy_grpc_call` を発行するモジュールが
存在しないためである。効果はメタデータ 1 ペアあたり 2 アロケーション + メッセージ本体 1 コピーの
削減という構造的なもので、単体テスト（正常・切り詰め・空入力の走査）で挙動同一性を担保している。
`grpc_h2_*` 構成での非退行はフルスイート計測で確認する。
