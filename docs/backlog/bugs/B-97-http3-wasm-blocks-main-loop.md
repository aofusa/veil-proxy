# B-97: HTTP/3 の WASM フィルタ実行がメインループを止め、同じワーカーの全 HTTP/3 接続を待たせる

**状態: 完了（feat/v080-limitations）**

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

## 範囲の拡張と改修方針（2026-10-09、feat/v080-limitations）

調査の結果、メインループを止めるのは Wasm だけではない。`process_h3_events` は**バッファ経路の要求をすべて**
`self.handle_request(...).await` でインライン実行するため、gRPC・h2c 上流・`buffering = full`・Wasm・
HTTPS 上流（B-105）・キャッシュ外の静的配信（offload）の待ちの間、同じワーカーの全 HTTP/3 接続が止まる。
ワーカーあたりのスループット上限は「1 ÷ 1 要求の所要時間」になる。

改修: バッファ経路の要求もストリーミング経路と同じアクターモデルにする。要求ごとにタスクを spawn し、
応答は `RespMsg`（`Head` / `Body` / `Trailers` / `Error`）のチャネルと `ConnWaker` でメインループへ返す。
メインループは await しない。B-105（スレッドを生成する HTTPS 経路）も同時に置き換える。

## 結果（2026-10-09）

- バッファ経路の要求処理（`handle_request` 以下、静的配信・メトリクス・gRPC・h2c 上流・Wasm・`buffering = full`）を
  quiche に触れない `H3BufferedTask` へ移した。応答は `RespMsg`（`Head` / `Body` / gRPC の `Trailers`）として
  チャネルへ流し、メインループの `drive_proxy_streams` が `ProxyStream`（要求方向を持たない `new_buffered`）で送出する。
  gRPC のトレーラーは `send_additional_headers(is_trailer_section=true, fin=true)`（StreamBlocked なら保留して再送）。
- **タスクとして spawn する方式は採らなかった**: メインループはイテレーション末尾の `yield_now` まで spawn 済みタスクを
  走らせないため、応答の送出が 1 イテレーション遅れ、`h3_file` 3B で −11.8%（6 ラウンド全敗・分布分離）だった。
  代わりにワーカー単位の `FuturesUnordered` に Future を置き、作成直後に 1 回 poll（`poll_buffered_once`）、
  待ちに入ったものは select のアーム（`poll_buffered_arm`）で起床時に進める。メインループは await しない。
- 応答の最後の断片・本文の無い HEADERS に fin を載せる（`Receiver::is_finished` で先読み）。空の fin を別送しない。
- `send_h3_head` を `h3::HeaderRef`（借用ヘッダ）にし、ヘッダごとの `Vec` 確保をなくした（ストリーミング経路にも効く）。
- 回帰テスト: E2E `test_b97_http3_buffered_request_does_not_block_connection`（同じ QUIC 接続で、上流 1.5 秒待ちの
  `buffering = full` 要求の間に出した別要求が 1 秒以内に返る）。旧実装では 1.31 秒待たされて失敗することを確認済み。

計測（交互 A/B、Docker、`h2load --alpn-list=h3 -c100 -m10`、base = 改修直前のコミット）:

| 構成 | base | new | 差 |
|---|---|---|---|
| `h3_file` 3B（6 ラウンド） | 56,262 | 55,170 | −1.9%（分布は重なる） |
| `h3_file` 54KB（4 ラウンド） | 1,867 | 1,886 | +1.1%（ノイズ） |
| `h3_proxy`（`buffering = full`）3B（4 ラウンド、base の外れ値 1 回を除く） | 16,259 | 16,100 | −1.0%（ノイズ） |

スループット計測では上流が速いため差は出ないが、遅い上流・オフロード・Wasm の待ちが同じワーカーの他の接続へ波及しなくなった
（上記 E2E）。
