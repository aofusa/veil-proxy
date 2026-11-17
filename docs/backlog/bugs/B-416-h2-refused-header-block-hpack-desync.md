# B-416: HTTP/2 のサーバが、ストリームのエラーで断る HEADERS のブロックを HPACK で読まずに捨てて接続を続けていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「HTTP/2 server drops HEADERS blocks on stream errors without HPACK-decoding them and keeps the connection open」
（同じ指摘が 2 件）から確認。

## 事象

- `http2::connection::Http2Connection::handle_headers` は、ストリーム単位で断る HEADERS（同時ストリーム数の超過、
  veil がリセットして表に残る閉じたストリームへのブロック、END_STREAM の無い 2 つ目の HEADERS、自分に依存する
  優先度、`max_header_block_size` の超過）を、唯一の `hpack_decoder.decode` に届く前に `Err(stream_error)` で
  返していた。`proxy::h2` のメインループはストリームのエラーには RST_STREAM を送って接続を続ける。
- 相手のエンコーダはそのブロックのフィールドを動的表に入れているので、同じ接続の後の要求のインデックスが veil では
  別のエントリを指す（RFC 9113 §4.3 はブロックを必ず読むか、接続を COMPRESSION_ERROR で閉じることを求める）。
- 前段の多重化プロキシの後ろでは、ある利用者が本文の上限を超える要求にトレーラを付けるだけで（veil が本文の上限で
  リセットした直後のトレーラが閉じたストリームへ届く）、同じ接続の別の利用者の要求のヘッダ（認証・利用者の識別）が
  ずれたエントリで読まれた。

## 対応

- ストリームのエラーで断るブロックは、このフレームでブロックが終わるなら HPACK で読んでからエラーを返す
  （`Http2Connection::reject_header_block`）。CONTINUATION へ続くブロックは読めないので接続のエラー
  （COMPRESSION_ERROR）にする。
- `max_header_block_size` の超過（HEADERS 単体・CONTINUATION の累積）は読まないので接続のエラー（ENHANCE_YOUR_CALM）
  にする（利用者ガイドは以前から「接続を閉じる」と書いていた）。
- 利用者ガイド（`protocols.md` の英日）に書いた。

## テスト

- `http2::connection::tests::refused_header_blocks_keep_the_hpack_table_in_sync`（閉じたストリームへのトレーラと
  自分に依存する優先度の要求を断った後、それらが表に入れたエントリをインデックスで参照する要求が正しいフィールドで
  読まれる。CONTINUATION へ続く断るブロックは接続のエラー）。
