# B-318: ヘッダ・ログへの CR/LF の注入と HTTP/2 の擬似ヘッダの数え溢れ

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F9・F15・F30・F36・F38・F40・F51）で判明。

- **F15**: 上流の HTTP/2 の応答のヘッダ（HPACK は任意のバイトを運べる）を、HTTP/1.1 のクライアントへそのまま
  書いていた（CR/LF で応答を分割できる）。
- **F30**: ネイティブハンドラの応答のヘッダ（デコード済みのクエリ・パスを返し得る）を HTTP/1.1 の head・101・
  chunked のトレーラへそのまま書いていた。
- **F40**: Proxy-Wasm のゲストが `proxy_set_header_map_pairs`・`proxy_add_header_map_value`・
  `proxy_replace_header_map_value` で書いたヘッダを検査せず、HTTP/1.1 の応答（と上流への要求）へ書いていた。
- **F9**: HTTP/2 の擬似ヘッダの重複の数が `u8` で、257 個の `:path` で 1 に戻り重複の検査を素通りした
  （リリースビルドは折り返す）。
- **F36**: GOAWAY のデバッグデータを warn のログへエスケープせずに書いていた（ログの行の偽造）。
- **F38**: Linkerd の `/metrics` の authority のラベルは `"` を消すだけで、末尾の `\` が表示形式を壊した。
- **F51**: メッシュのアクセスログの `%REQUESTED_SERVER_NAME%` に ClientHello の SNI をそのまま書いていた。

## 修正

- `http_utils::is_writable_h1_field`（名前が token、値に CR・LF・NUL が無い）を HTTP/1.1 へ書く箇所
  （ハンドラの head・101・トレーラ、Wasm が書き換えた応答のヘッダ）で使い、満たさないヘッダは書かない。
- 上流の HTTP/2 の応答は、満たさないフィールドがあればストリームを PROTOCOL_ERROR でリセットする（RFC 9113 §8.2.1）。
- Wasm のホスト関数は、満たさないヘッダ（擬似ヘッダの `:` は許す）を BadArgument で断る。
- 擬似ヘッダの数は飽和加算。GOAWAY のデバッグデータは 256 バイトまでを `escape_ascii` で書く。
- Prometheus のラベルの値は `\`・`"`・改行をエスケープ。アクセスログの SNI は制御文字を `\xNN` にする。

単体試験 `http_utils::tests::writable_h1_field`・`wasm::host::headers::tests::guest_field_ok_rejects_crlf`・
`mesh::linkerd::metrics::tests::label_value_is_escaped`。

## 関連

- docs/artifacts/security/v0110-claude-security/（F9・F15・F30・F36・F38・F40・F51）
