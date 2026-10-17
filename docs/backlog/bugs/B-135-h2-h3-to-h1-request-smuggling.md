# B-135: HTTP/2・HTTP/3 の要求を HTTP/1.1 の上流へ送るとき、要求の密輸ができた

**状態: 完了（feat/v080-release、2026-10-17。F-236 の監査の F1・F2）**

## 症状

HPACK・QPACK はフィールドに任意のバイトを運べる。veil は HTTP/2・HTTP/3 で受けたフィールドを検査せずに、HTTP/1.1 の上流への
要求に書いていた。

- 値に CR・LF を入れると、上流には別のヘッダ・別の要求として届く（`x-a: v\r\nTransfer-Encoding: chunked` 等）。
- `:method`・`:path` に空白・CR・LF を入れると、要求行を書き換えられる。
- メッシュ（`src/mesh/h2.rs` の `to_h1`）は、クライアントの `content-length` を本文と照らさずにそのまま送っていた。

プール接続の上流では、密輸した要求がほかの利用者の応答と入れ替わる。

## 修正

- HTTP/2（`validate_request_headers`）・HTTP/3（`h3_check_field_section`）で RFC 9113 §8.2.1・RFC 9114 §4.2 を検査する:
  名前は token、値は NUL・CR・LF を含まず空白で始まらず終わらない、`:method` は token、`:path` は空白・制御文字を含まない。
  違反はストリームのエラー（HTTP/2 は PROTOCOL_ERROR、HTTP/3 は H3_MESSAGE_ERROR）。名前の小文字化の確保も無くした。
- HTTP/1.1 の上流へ書くときは `proxy::write_h1_forward_headers` 1 つを通す（疑似ヘッダ・ホップバイホップ・`content-length`・
  `host`・`expect`・不正な名前と値を書かない）。HTTP/2・HTTP/3・ストリーミングの全経路がこれを使う。
- メッシュの `to_h1` は `content-length` を解釈し、読めない・本文と合わない要求を 400 で断る（上流の接続は捨てる）。

## 試験

`http2::connection::tests::request_headers_reject_smuggling_fields`・
`http3_server::tests::test_h3_field_section_rejects_smuggling_fields`・
`proxy::path_tests::h1_forward_headers_apply_policy_and_drop_smuggling`。

## 関連

F-236、B-136、B-100（h3 のフィールドの検査）。
