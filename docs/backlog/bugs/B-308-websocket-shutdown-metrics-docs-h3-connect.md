# B-308: WebSocket のトンネルが graceful shutdown で閉じない・メトリクスに出ない、文書・feature 定義の食い違い、HTTP/3 の素の CONNECT が `/` へルーティングされる

**状態: 未修正（2026-11-05 起票。コードレビューで発見。行番号は f8dc80de）**

## 事象（コードの読み取り）

- **W-9 shutdown**: graceful shutdown（F-187）でプロキシのトンネルを閉じず、drain の時間切れまで待つ（ハンドラの WebSocket は 1001 で閉じる）。
- **W-10 メトリクス**: `veil_proxy_websocket_connections` はハンドラだけを数える（`src/api/ws/mod.rs:84-95`）。HTTP/1.1 のプロキシのトンネルは 101 の要求 1 件として
  だけ見え、所要時間のヒストグラムを歪める。
- **W-11 文書・定義**:
  - `websocket` feature（`Cargo.toml:309-312`）は何も切り替えないのに `building.md:111` が挙げている。
  - F-288 は `performance.md` に「WebSocket は HTTP/1.1 だけ」と書いたとしているが、英日とも記述が無い。
  - 文書の「バイパス転送」は実際にはユーザー空間のバッファでコピーしている（[B-306](B-306-websocket-relay-drops-data-iouring.md) で splice にするなら文書はそのまま）。
  - `use_h2c`・h2 の上流でも WebSocket は HTTP/1.1 で張ることを書いていない。
  - ハンドラは GET 以外に 405（`ws/mod.rs:348`）を返すが、`library.md:441` は 400・426 だけを挙げている。
  - permessage-deflate は最初の `Sec-WebSocket-Extensions` の行しか見ない。
- **W-12 HTTP/3 の素の CONNECT**: `src/h3_fields.rs:105-112` が受け付け、`http3_server/mod.rs:1503-1504` がパスを `/` にするので、`CONNECT /` として上流へ
  転送され得る。HTTP/2 は同じ要求を（メッシュのフラグが無ければ）拒否する。

## 改修案

- shutdown でトンネルを閉じる（WebSocket の close フレームは中継では作れないので TCP を閉じる。ハンドラと同じく drain の開始で）。
- 別のゲージ `veil_proxy_websocket_tunnels{protocol}` を足し、トンネルの要求はレイテンシのヒストグラムから外すか別ラベルにする。文書（`observability` 系・英日）。
- 文書の食い違いを英日で直す。`websocket` feature は削除するか意味を持たせる（`--no-default-features` の組み合わせで確認）。`Sec-WebSocket-Extensions` は全行を見る。
- HTTP/3 の `:protocol` 無しの CONNECT は、メッシュの用途が無い限り H3_MESSAGE_ERROR か 405（[F-288](../features/F-288-websocket-over-h2-h3.md) の CONNECT の分岐で扱う）。

## 関連

- [F-288](../features/F-288-websocket-over-h2-h3.md)、[B-306](B-306-websocket-relay-drops-data-iouring.md)、[B-307](B-307-websocket-upgrade-handling.md)、[F-187](../features/F-187-graceful-shutdown-close-idle-keepalive.md)
