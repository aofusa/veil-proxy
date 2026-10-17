# B-136: HTTP/2・HTTP/3 の要求に `remove_request_headers`・`add_request_headers` が効いていなかった

**状態: 完了（feat/v080-release、2026-10-17。F-236 の監査の F4）**

## 症状

`[route.security]` の `remove_request_headers`（クライアントの送る信頼のヘッダ、`x-internal-user` 等を消す）と
`add_request_headers`（`$client_ip` 等を付ける）は、HTTP/1.1 のクライアントの経路でしか使われていなかった。
HTTP/2・HTTP/3 のクライアントは、消すはずのヘッダをそのまま上流へ送れた（上流 HTTP/1.1・h2c・HTTPS の h2・ストリーミングの多重化のどれも）。

## 修正

`proxy::forward_allowed`（削除の指定）と `proxy::expanded_added_headers`（追加の展開。値の CR・LF・NUL は送らない）を
作り、HTTP/2・HTTP/3 から上流への全経路（HTTP/1.1 の書き出し・h2c・HTTPS の h2・`upstream_mux`・HTTP/3 のバッファ経路と
ストリーミング経路）で使う。設定が空なら分岐 1 つで、確保しない。

## 試験

`proxy::path_tests::h1_forward_headers_apply_policy_and_drop_smuggling`。E2E（HTTP/2・HTTP/3 の既存の試験）。

## 関連

F-236、B-135。
