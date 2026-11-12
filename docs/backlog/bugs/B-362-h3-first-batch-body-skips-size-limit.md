# B-362: HTTP/3 で見出しと同じ束で届いた要求本文が `max_request_body_size` を素通りしていた

**状態: 修正済み（feat/beat-nginx、2026-11-12）**

機能とプロトコルの行列の試験（`tests/standalone/matrix_tests.rs` の `feature_protocol_matrix`）で見つけた。

## 事象

HTTP/3 のストリーミングの中継（`Decision::Stream`）・ハンドラ（`Decision::StreamHandler`）は、要求本文の上限
（`[route.security] max_request_body_size`）を、pump の `recv_body` の後でだけ検査していた。見出しと同じ受信の束で
届いた本文は分類の前に `stream_bodies` へ読み出され、分類の後にそのまま `req_pending` へ積まれて上流へ送られる。
そのため本文の全量が最初の束で届くと、上限の検査を一度も通らなかった。

- `Content-Length` の付いた要求は分類の `check_security` で 413 になるので影響しない。`Content-Length` を省いた要求
  （HTTP/3 では省略できる）だけが素通りする。
- 最初の束で読む量の上限は全ルートの最大の上限（`l7::max_buffered_request_body`）とストリームのフロー制御の窓なので、
  ルートの上限より大きな本文を上流へ送れた。
- 再現: `max_request_body_size = 1024` のルートへ `Content-Length` 無しの 4096 バイトの POST を HTTP/3 で送ると、
  上流の 200 が返る（HTTP/1.1・HTTP/2 は 413）。

## 対応

- 最初の束の本文を取り込む処理を `ProxyStream::take_first_body` にまとめ、取り込んだ時点で上限を検査する。超えていたら
  バックエンドを起動せず、受信を `STOP_SENDING(H3_NO_ERROR)` で止め、`drive_response_flush` が 413 を返す。
- pump の上限超過の `STOP_SENDING` のエラーコードも 0 から `H3_NO_ERROR`（RFC 9114 §4.1.1）にそろえた。

## テスト

- `tests/standalone/matrix_tests.rs` の「request body limit (413)」（5 つのプロトコルで `Content-Length` 無しの 4096 バイトの
  POST が 413 になること。修正前は HTTP/3 だけ 200 で失敗した）。
