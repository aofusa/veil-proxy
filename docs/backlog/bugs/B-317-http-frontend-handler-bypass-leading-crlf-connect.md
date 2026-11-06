# B-317: データプレーンの HTTP の受け口の検査漏れ（ハンドラのストリーミング経路・先頭の空行・CONNECT・Content-Length）

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F5・F6・F7・F8・F14・F16・F17・F18）で判明。

- **F5・F6（HTTP/2 のハンドラのストリーミング経路）**: `h2_route_streaming_plan` は `streaming_request` の
  ハンドラを `[route.security]`（IP・メソッド・レート制限）と Proxy-Wasm の要求のフェーズを通さずに呼んでいた。
  HEADERS に END_STREAM を付けないだけ（gRPC の普通の呼び出し）で、HTTP/1.1・HTTP/3・バッファ経路では
  拒否されるルートへ届いた。
- **F14（HTTP/3 のハンドラのストリーミング経路）**: Proxy-Wasm のあるルートでもストリーミングの経路を選び、
  Wasm の認可を飛ばしていた。
- **F16（拡張 CONNECT の WebSocket ハンドラ）**: HTTP/2・HTTP/3 の拡張 CONNECT（RFC 8441・9220）だけ、
  ハンドラのルートで Proxy-Wasm の要求ヘッダの段を飛ばしていた。
- **F7・F8（HTTP/1.1 の先頭の空行）**: ヘッダの大きさの検査は最初の CRLFCRLF の位置で測るが、httparse は
  要求行の前の空行を飛ばして読む。`\r\n\r\n` で始まる要求は 8KB の上限をすり抜け、蓄積が際限なく伸びた（F8）。
  ヘッダの位置を u16 に詰めていたので、64KB を超える位置のヘッダは別のバイトを指した（F7）。
- **F17（HTTP/1.1 の Content-Length）**: `parse::<usize>()` は `+10` を受け付けた（上流は別の解釈をし得る）。
- **F18（HTTP/1.1 の CONNECT）**: `origin_form_target` は CONNECT なら何でも通し、`CONNECT x/admin/users` は
  正規化されずに catch-all のルートに合って上流へそのまま送られた。Go の net/http 等は `/admin/users` と読む。

## 修正

- HTTP/2: 検査で拒否される要求と Wasm のあるルートはバッファ経路へ回す（そこで検査・Wasm を通す）。
- HTTP/3: Wasm のあるルートはバッファ経路へ回す。
- 拡張 CONNECT: ハンドラのルートでも `wasm_request_headers` を通し、終わったら `on_request_complete_async`。
- HTTP/1.1: 要求行の前の空行（RFC 9112 §2.2）を検査の前に捨てる（捨てる量は 8KB まで、超えたら 400）。
  httparse の読んだヘッダの長さでも 8KB を確かめる（超えたら 431）。
- Content-Length は `http_utils::parse_content_length`（数字だけ・20 桁まで）で読む。
- CONNECT の authority 形式は 400（HTTP/1.1 の CONNECT のトンネルは持たない。メッシュの HTTP/1.1 も同じ関数）。

試験: 単体 `http_utils::tests::origin_form_target_rejects_connect_authority_form`、E2E
`security_audit::test_b317_connect_authority_form_rejected`・`test_b317_leading_empty_lines_do_not_bypass_header_limit`。

## 関連

- docs/artifacts/security/v0110-claude-security/（F5・F6・F7・F8・F14・F16・F17・F18）
- B-315（メッシュの HTTP/1.1 の要求先）
