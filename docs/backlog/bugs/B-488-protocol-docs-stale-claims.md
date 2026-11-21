# B-488: プロトコルの文書の誤り（HTTP/2 Server Push・ハンドラのストリーミング・拡張 CONNECT の upgrade）

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

## 出典

機能とプロトコルの対応の確認（`docs/artifacts/protocol-parity/audit.md` の #8）。

## 事象

- `protocols.md`（英日）の HTTP/2 の機能表に「Server Push」がある。veil はサーバープッシュを実装していない
  （`SETTINGS_ENABLE_PUSH` を 0 で広告し、`PUSH_PROMISE` を送らない）。主要なブラウザもプッシュを外した。
- `library.md`（英日）:
  - 「ストリーミングの要求本文は HTTP/2 だけ届いた順に渡し、HTTP/1.1・HTTP/3 は全部読んでから 1 塊」→ HTTP/3 は F-209 #5、
    HTTP/1.1 は F-266（`Content-Length` の本文）で届いた順に渡す。HTTP/1.1 の chunked だけが 1 塊（F-315）。
  - 「`max_body_size` は HTTP/2 のストリーミングのルートに効かない」→ ストリーミングのルートは 3 プロトコルとも本文の大きさを見ない
    （F-266 の方針）。
  - upgrade の節の「HTTP/2・HTTP/3 で届いたら 400（拡張 CONNECT は非対応）」→ F-288 で拡張 CONNECT の upgrade に対応済み
    （同じ文書の後ろの節と食い違う）。

## 改修案

- 上の 3 点を英日で直す。

## 関連

- [F-288](../features/F-288-websocket-over-h2-h3.md)、[F-266](../features/F-266-multipart-file-upload.md)、[F-209](../features/F-209-grpc-handler-gap-audit.md)
