# B-144: WebSocket のアップグレードの要求に `remove_request_headers`・`add_request_headers` が効いていなかった

**状態: 完了（feat/v080-release、2026-10-18。F-236 の再走査の F2）**

## 症状

`Connection: Upgrade` と `Upgrade: websocket` を付けた要求は、Proxy のルートなら `handle_websocket_proxy` へ渡り、そこでは
Host と不正な名前・値だけを除いてクライアントのヘッダを全部送っていた。削除の指定（`x-user-id` 等の信頼のヘッダ）も、追加の
指定（`X-Real-IP` 等）も効かず、偽のヘッダが上流へ届いた（上流がアップグレードしなくても）。

## 修正

`forward_allowed` と `expanded_added_headers`（B-136）を WebSocket の経路でも使う。Connection と Upgrade 以外のホップバイホップ
（keep-alive・transfer-encoding・proxy-connection・te）も送らない。

## 関連

F-236、B-136、B-145。
