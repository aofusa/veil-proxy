# F-316: `Upgraded::into_tokio_io()` を HTTP/2・HTTP/3 の拡張 CONNECT の upgrade でも使えるようにする

**状態: 完了（2026-11-22、feat/protocol-parity。2026-11-21 起票。機能 × プロトコルの網羅性の確認）**

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

## 対応

- コードは F-288 の時点で既に動いていた。`into_tokio_io()` の中継のタスク（`api::upgrade::tokio_io::pump`）は `Upgraded` の
  `readable`・`read`・`write_all`・`shutdown` だけを使い、それらは HTTP/1.1 の接続と HTTP/2・HTTP/3 のストリーム（`UpIo::Stream`。
  読みは要求の本文のチャネル、書きは `TunnelOut::body`、`shutdown` は送り先を閉じる＝END_STREAM・FIN）の両方に対応している。
  「HTTP/1.1 だけ」は文書（`library.md`）の記述が古かった。
- 文書（library、英日）を直し、ストリームの上で動くことを試験で確かめた。

### 試験

- 単体 `proxy::ws_tunnel::tests::tokio_io_echo_on_extended_connect_streams`: `Response::switching_protocols` の関数の中で
  `into_tokio_io()` の上のエコー（`AsyncReadExt::read`・`AsyncWriteExt::write_all`・`shutdown`）を書き、HTTP/2（`TunnelOut::H2`）と
  HTTP/3（`TunnelOut::H3`）のストリームに `run_on_stream`（拡張 CONNECT の実際の入口）で載せる。要求の本文の 3 つの断片が
  応答の本文に同じ順で返り、`shutdown` で応答の送り先が閉じる。
- ライブラリの E2E（実際の h2c・HTTP/3 のクライアントからの拡張 CONNECT）は入れていない。試験の HTTP/3 クライアント（`h3` 0.0.8）は
  `:protocol` を `webtransport`・`connect-udp` しか送れず（F-288 の「範囲外・残り」と同じ）、HTTP/2 も試験クライアントに拡張 CONNECT の
  口が無い。拡張 CONNECT の受け付けから `run_on_stream` までは F-288 の E2E（ハンドラ以外のルート）と単体で見ている。

## 関連

- [F-261](F-261-handler-http11-upgrade.md)・[F-288](F-288-websocket-over-h2-h3.md)
