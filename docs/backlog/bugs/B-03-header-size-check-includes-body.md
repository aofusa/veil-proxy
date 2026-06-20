# B-03: ヘッダーサイズチェックにボディバイトが含まれる

## 事象（再現手順）

1. 大きなリクエストボディ（8KB 超）を送信する
2. プロキシが 431 Request Header Fields Too Large を返す
3. 実際のヘッダーは 8KB 以内だがボディが含まれた accumulated バッファ全体でチェックしていた

```
POST /h2c/large.txt HTTP/1.1
Content-Length: 100000
...（ヘッダーは 200 バイト程度）

[100KB のボディ]
```

## 影響

- H2C フローコントロールテスト (`test_h2c_flow_control`) が 431 を返して失敗
- H2C 大容量リクエストボディテスト (`test_h2c_large_request_body`) が 431 を返して失敗
- 実際のヘッダーサイズが制限内でも誤拒否される

## 調査メモ

`handle_requests` の HTTP ヘッダー読み取りループ内で：

```rust
// 修正前（誤り）
if accumulated.len() > MAX_HEADER_SIZE {
    // accumulated はヘッダー後のボディも含む
}
```

`accumulated` は TLS ストリームから読んだ生バイト列で、
`\r\n\r\n` 以降のボディ先頭バイトも含まれる。
ヘッダーサイズは `\r\n\r\n` までの長さで判断すべき。

## 改修案・対応内容

```rust
// 修正後
let header_section_end = accumulated.windows(4).position(|w| w == b"\r\n\r\n");
let header_check_size = header_section_end.map_or(accumulated.len(), |end| end + 4);
if header_check_size > MAX_HEADER_SIZE {
    // ヘッダー部分のみでチェック
}
```

## 完了日

2026-06-20（本セッション対応済）
