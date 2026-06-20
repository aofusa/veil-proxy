# B-04: WASM レスポンスフィルタが HTTPS バックエンドパスに未適用

## 事象（再現手順）

1. HTTPS バックエンドへのプロキシルートに WASM モジュールを設定する
2. リクエストを送信する
3. WASM フィルタが追加すべきヘッダー（`X-Veil-Processed: true` 等）がレスポンスに付与されない

## 影響

- HTTPS バックエンドへのプロキシ経由のレスポンスに WASM フィルタが適用されない
- `transfer_https_response_with_compression` 関数に WASM ヘッダーフィルタブロックが欠落

## 調査メモ

HTTP バックエンドパス（`transfer_response_with_compression`）には WASM レスポンスヘッダーフィルタが実装されていたが、
HTTPS バックエンドパス（`transfer_https_response_with_compression`）には未実装だった。

## 改修案・対応内容

`transfer_https_response_with_compression` に同等の WASM レスポンスヘッダーフィルタブロックを追加した。
HTTP パスと同じロジックを HTTPS パスにも適用することで動作を統一した。

## 完了日

2026-06-20（本セッション対応済）
