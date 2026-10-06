# B-97: HTTP/3 の WASM フィルタ実行がメインループを止め、同じワーカーの全 HTTP/3 接続を待たせる

**状態: 未対応（設計変更が必要）**

## 事象

FreeBSD aarch64 の E2E で、`test_http3_quic_keepalive_idle`（12 秒アイドル後のリクエストが
10 秒以内に返らない）・`test_http3_udp_unreachable_fallback`・`test_http3_wasm_*` が
並行実行時にだけタイムアウトした（単独では通る）。

## 原因

`process_h3_events` は新規リクエストを `self.handle_request(...).await` で**メインループの
タスク内でインライン実行**する。WASM フィルタ（`on_request_headers_with_modules` 等）は
`call_async` + `fuel_async_yield_interval` で協調的に yield するが、yield で動けるのは
**他のタスク**（バックエンドタスク等）だけで、await しているメインループ自身は WASM の完了まで
進まない。したがって WASM の実行が長いと、同じワーカーの**全 HTTP/3 接続**
（受信・ACK・他ストリームの応答）がその間止まる。

ネイティブコード（Cranelift）の環境では WASM 実行は µs〜ms なので表面化しないが、
Pulley インタープリタで動くターゲット（NetBSD 全アーキ・FreeBSD/OpenBSD aarch64、
B-55）では桁違いに長くなる。特にデバッグビルドでは WAF モジュール 1 回が 22 秒かかり
（B-98）、E2E が並行実行時にタイムアウトしていた。B-98 で E2E は安定したが、
構造上の問題（WASM 実行中のヘッドオブラインブロッキング）は残る。

HTTP/1.1・HTTP/2 はリクエストごと（ストリームごと）にタスクが分かれているので影響を受けない。

## 改修案

WASM を通るリクエストの「ヘッダフェーズ」を、バックエンドタスクと同じく**別タスクへ spawn**
し、結果（続行 + 変更後ヘッダ / ローカルレスポンス）をチャネルと `ConnWaker` でメインループへ
返す。メインループはその間ほかの接続・ストリームを処理し、結果到着でダーティ化して
送出を再開する（F-32 のアクターモデルと同じ形）。`handle_request_impl` は WASM の
結果を受けてからの分岐（静的配信・プロキシ・ローカルレスポンス）が多いので、
「WASM 前段（spawn）→ 結果を `pending_wasm` に保持 → 次パスで後段を実行」の 2 段に分ける。
