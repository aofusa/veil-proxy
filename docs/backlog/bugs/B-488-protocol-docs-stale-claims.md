# B-488: プロトコルの文書の誤り（HTTP/2 Server Push・ハンドラのストリーミング・拡張 CONNECT の upgrade）

**状態: 修正済み（feat/protocol-parity、2026-11-21）**

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

## 対応（2026-11-21）

- `protocols.md`（英日）の HTTP/2 の機能表から「Server Push」を外し、実装していないこと（`PUSH_PROMISE` を送らない・
  `SETTINGS_ENABLE_PUSH = 0`）を書いた。
- `library.md`（英日）: ストリーミング受信は HTTP/2・HTTP/3 が全二重、HTTP/1.1 は `Content-Length` の本文を届いた順に渡して応答は
  本文の後、chunked は 1 断片。`max_body_size` はどのプロトコルのストリーミングのルートにも掛からない。upgrade の 400 の条件から
  「HTTP/2・HTTP/3 で届いた」を外し、`:protocol` の一致する拡張 CONNECT だけがアップグレードできると書いた。
