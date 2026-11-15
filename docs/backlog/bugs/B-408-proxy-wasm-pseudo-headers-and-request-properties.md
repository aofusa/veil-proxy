# B-408: Proxy-Wasm のゲストから `:path` 等の疑似ヘッダが読めず、`request.host`・`request.scheme`・`request.protocol` が違っていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

B-406（WAF の例のフィルタ）を調べる途中で見つけた。

- `get_http_request_header(":path")` 等が HTTP/1.1・HTTP/3 では常に無く、HTTP/2 では `:scheme` だけが見えた（HTTP/1.1・HTTP/3 は
  疑似ヘッダを除いてゲストへ渡し、HTTP/2 は `:method`・`:path`・`:authority` を除いた残りを渡していた）。利用者ガイドの Rust の
  例（`get_http_request_header(":path")`）も、`docs/guide/migration-envoy.md` の「Envoy 向けの Rust・C++・TinyGo のフィルタが
  そのまま動く」も成り立っていなかった。Envoy 向けのフィルタはパスを `:path` で読むので、パスで判断するフィルタは黙って素通りする。
- 応答ヘッダの表にも `:status` が無かった。
- プロパティ `request.host` は `Host` ヘッダだけを見ていたので HTTP/2・HTTP/3 では取れず、`request.scheme` は常に `https`、
  `request.protocol` は常に `HTTP/1.1` だった。
- 応答の段と `on_log` の context には要求のメソッド・パスが無く、`request.path` も空だった。
- ゲストは `:` で始まる名前のヘッダを足せた。保持するヘッダに入り、HTTP/1.1 の上流への要求に不正なヘッダの行として書かれ得た。

## 修正

- 要求ごとの状態（`RequestSessions`）に要求の属性（`wasm::RequestInfo`: `:authority`・`:scheme`・プロトコル）を置く。HTTP/1.1
  （`Host`、TLS かどうか、`HTTP/1.0`・`HTTP/1.1`）・HTTP/2（`:authority`、要求の `:scheme`）・HTTP/3・HTTP/2 と HTTP/3 の
  WebSocket（拡張 CONNECT）の経路が、モジュールを走らせる前に置く。要求ヘッダの段は要求のメソッドとパスも置き、応答の段・
  `on_log` の context へ写す。
- 属性を持つ context では、ホストが要求ヘッダの表に `:authority`・`:path`・`:method`・`:scheme`、応答ヘッダの表に `:status` を
  context から作って先頭に見せる（Envoy と同じ）。保持するヘッダには入れない（上流・クライアントへ送るヘッダと混ざらない）。
- 疑似ヘッダの書き換え・追加・削除は `BadArgument`。今と同じ値に置くこと（読んだ表をそのまま `set_http_request_headers` で
  戻す往復）は受け付けて何も変えない。`:` で始まるそれ以外の名前は足せない。
- 属性を置かない呼び出し元（メッシュのデータプレーン。疑似ヘッダを自分で表に入れ、ゲストが書き換えた `:path` 等を上流へ
  反映する）は従来どおり表をそのまま見せて書かせる。
- `request.host` は `:authority`（属性が無ければ表の `:authority`・`Host`）、`request.scheme`・`request.protocol` は属性から。
- `proxy_replace_header_map_value` も読み取り専用・未知の MapType を他の書き込みと同じく `BadArgument` にした。
- 利用者ガイド（`docs/guide/wasm.md`「Pseudo-headers and Request Properties」・`migration-envoy.md` と `ja`）に書いた。

応答の段からは、疑似ヘッダ以外の要求ヘッダは今も見えない（ガイドに書いた。必要なフィルタは `instance_reuse = true` で
HTTP の context に持つ）。

## 試験

- 適合試験 `tests/proxy_wasm_conformance.rs`: `b408_pseudo_headers_are_visible_and_read_only`（読み取り・並び・書き換えの拒否・
  往復）、`b408_caller_supplied_pseudo_headers_stay_writable`（メッシュの形）。
- 単体: `wasm::tests::b408_pseudo_header_phase_tests::response_phase_sees_request_path_and_status`（応答の段の `:path`・
  `:status`。`instance_reuse` の有無の両方）、`wasm::host::properties::tests::b408_request_host_scheme_protocol_follow_request_info`。
- E2E `coverage::test_http3_wasm_local_response`（B-406）は WAF の例が `:path` のクエリのパストラバーサルで 403 を返すことを見る。
