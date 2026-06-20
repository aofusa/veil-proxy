# B-02: body timeout 後 408 を送信しても接続を閉じない

## 事象（再現手順）

`client_body_timeout_secs` によるタイムアウトで 408 を返した後、プロキシが keep-alive として次リクエスト待ちに戻る。クライアントは `Connection: close` を受信したにもかかわらず FIN を待ち続け、読み込みタイムアウトが切れるまでブロックする。

再現テスト: `test_buffering_client_write_timeout`（408 は届くが、クライアント側 read が 10 秒タイムアウトするため ntest の 15 秒タイムアウト内に完了しない）

## 影響

- 408 を返しても接続スロットを長時間占有する。
- テストが ntest タイムアウト（15 秒）で失敗する。

## 調査メモ

`proxy_https_request_with_compression` が `Some((408, 0, false))` を返した後、呼び出し元 `proxy_https_pooled` では `client_wants_close`（リクエストの Connection ヘッダー由来）だけを参照して `should_close` を決定していた。テストの POST リクエストには `Connection: close` がないため `should_close = false` となり、接続が維持されていた。

`ERR_MSG_REQUEST_TIMEOUT` レスポンスには `Connection: close` ヘッダーが含まれているが、プロキシ側で接続を閉じていなかった。

## 改修案（実施済み）

`proxy_https_pooled`（および HTTP 版の `proxy_pooled`）の match ブランチで、status_code が 408 の場合は強制的に `should_close = true` にする。

```rust
let should_close = client_wants_close || status_code == 408;
Some((client_stream, status_code, total, should_close))
```

## 対応状況

完了（2026-06-20）。`src/main.rs` の `proxy_https_pooled` / HTTP 版 pooled 関数を修正。
