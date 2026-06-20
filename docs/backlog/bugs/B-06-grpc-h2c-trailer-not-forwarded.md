# B-06: gRPC H2C レスポンストレーラーが HTTP/1.1 クライアントに転送されない

## 事象（再現手順）

1. gRPC リクエストを HTTP/1.1 クライアントからプロキシ経由で H2C バックエンドに送信する
2. バックエンドが HTTP/2 トレーラー（`grpc-status`, `grpc-message`）を返す
3. クライアントがトレーラーを受信できない

## 影響

- gRPC エラーステータスがクライアントに伝わらない
- `test_grpc_invalid_frame`, `test_grpc_malformed_protobuf` 等が失敗

## 調査メモ

`proxy_h2c` 関数は HTTP/2 レスポンスを受け取り、HTTP/1.1 にダウングレードして
クライアントに返す。HTTP/2 トレーラーフレームの内容を HTTP/1.1 ヘッダーとして
転送するコードが欠落していた。

gRPC のプロトコル仕様：アプリケーションレベルのエラーは HTTP 200 + トレーラーの
`grpc-status` フィールドで伝達される（HTTP 4xx ではない）。

## 改修案・対応内容

`proxy_h2c` 内の HTTP/1.1 レスポンス構築部分に、`response.trailers` を
HTTP/1.1 ヘッダーとして追加するコードを挿入した：

```rust
for (name, value) in &response.trailers {
    http11_response.extend_from_slice(name);
    http11_response.extend_from_slice(b": ");
    http11_response.extend_from_slice(value);
    http11_response.extend_from_slice(b"\r\n");
}
```

## 完了日

2026-06-20（本セッション対応済）
