# B-482: 要求ヘッダの上限がプロトコルで違う（HTTP/2 は 8 KB を見ない）・`[route.security] max_request_header_size` が効かない

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

## 出典

機能とプロトコルの対応の確認（`docs/artifacts/protocol-parity/audit.md` の #2）。

## 事象

- veil の要求ヘッダの上限（`MAX_HEADER_SIZE` = 8 KB）は、HTTP/1.1（ワイヤ上の大きさ、`proxy/h1.rs`）と HTTP/3（名前と値の
  合計、F-101。`http3_server/mod.rs`・`request_task.rs`）では 431（`:path` が上限を超えれば 414）になるが、HTTP/2 は見ない。
  同じ要求（例: 10 KB の Cookie）が HTTP/2 だけ通り、HTTP/1.1・HTTP/3 では 431 になる。`configuration.md` の
  `[http3] max_field_section_size` は「veil の要求ヘッダの上限（8 KB）を超えれば 431」と書いている。
- ヘッダの数も、HTTP/1.1 は 64 個まで（httparse の格納先。超えると 400）、HTTP/2・HTTP/3 は数を見ない。
- `[route.security] max_request_header_size`（`routing.md` の表で「既定 8KB」）は設定に読むだけで、どのプロトコルでも
  使っていない。
- HTTP/3 は 8 KB 超えの要求ごとに `warn!` を出す（クライアントが起こせる。B-235 の方針では debug!）。

## 改修案

- HTTP/2 でも HTTP/3 と同じく、デコード後のヘッダの名前と値の合計が 8 KB を超えたら 431（`:path` が 8 KB を超えれば 414）。
  数を数えるのと同じ走査で、正規のヘッダ（擬似ヘッダ以外）が 64 個を超えたら 431 にする（HTTP/2・HTTP/3）。
  HTTP/1.1 の 64 個超えも 400 ではなく 431 にそろえる。
- `[route.security] max_request_header_size` をルートの上限として 3 プロトコルで効かせる（ルートの解決の後に、上の大きさが
  ルートの値を超えれば 431）。既定は veil の上限と同じ 8 KB（既定の構成の挙動は変わらない）。8 KB より大きくしても
  veil の上限（8 KB）は超えられないことを文書に書く。
- HTTP/3 の 8 KB 超えのログを debug! にする。

## 受け入れ条件

- 単体・統合: 8 KB を超えるヘッダの要求が HTTP/1.1・HTTP/2（h2c・TLS）・HTTP/3 のどれでも 431。`max_request_header_size = 1024`
  のルートは 2 KB のヘッダで 431、他のルートは通る。65 個のヘッダは 3 プロトコルとも 431。
- 文書（routing・security・configuration、英日）。

## 関連

- [F-101](../features/F-101-http3-web-features-flow-control.md)、[B-235](B-235-http3-warn-per-invalid-datagram.md)、
  [B-255](B-255-vendored-quiche-h3-frame-length-preallocation-ghsa-4fgf.md)
