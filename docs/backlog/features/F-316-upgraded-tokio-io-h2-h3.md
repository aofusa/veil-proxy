# F-316: `Upgraded::into_tokio_io()` を HTTP/2・HTTP/3 の拡張 CONNECT の upgrade でも使えるようにする

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

## 今の状態

- ハンドラの upgrade（`Response::switching_protocols`）は HTTP/1.1 の 101 と、HTTP/2・HTTP/3 の拡張 CONNECT（F-288）で動く。
- tokio の I/O（`AsyncRead`・`AsyncWrite`）として使う `Upgraded::into_tokio_io()`（`tokio-compat`）は HTTP/1.1 だけで、
  HTTP/2・HTTP/3 のストリームの upgrade では使えない（`library.md`）。tokio の上のプロトコルの実装（tokio-tungstenite 等）を
  HTTP/2・HTTP/3 の上で動かせない。

## 改修内容

- HTTP/2・HTTP/3 のストリームの `Upgraded`（チャネルで読み書きする形）にも `into_tokio_io()` を実装する。HTTP/1.1 の版と同じ
  橋渡し（`tokio-compat-bridge`）の上で、読みはストリームの DATA、書きは DATA の送信、`shutdown` はストリームの終わり。

## 受け入れ条件

- ライブラリの試験: HTTP/2（h2c）と HTTP/3 の拡張 CONNECT の upgrade で、`into_tokio_io()` の上のエコーが動く。
- 文書（library、英日）。

## 関連

- [F-261](F-261-handler-http11-upgrade.md)・[F-288](F-288-websocket-over-h2-h3.md)
