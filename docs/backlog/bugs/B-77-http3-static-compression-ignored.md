# B-77: HTTP/3 の静的配信で `[route.compression]` が一切効いていなかった

## 事象

HTTP/3（quiche）経路の **File / SendFile / MemoryFile バックエンド**では、ルートに
`[route.compression] enabled = true` を設定し、クライアントが `accept-encoding` を
送っていても、**レスポンスが一切圧縮されない**（`content-encoding` ヘッダも付かない）。

HTTP/1.1・HTTP/2 の同一設定では圧縮される。プロキシ経路（`Backend::Proxy`）は
HTTP/3 でも圧縮される。**静的配信だけが HTTP/3 で圧縮されない。**

## 原因

`src/http3_server.rs` のリクエスト処理が、ルート解決の結果を

```rust
let (prefix, backend, _route_compression) = match backend_result { ... };
```

と受けており、**圧縮設定を `_route_compression` として破棄していた**。
`Backend::Proxy` 側は別途 `resolve_http3_compression_config` を呼んでいたため
動いていたが、`File`/`SendFile`/`MemoryFile` の分岐には圧縮ネゴシエーション
（`should_compress` → `content-encoding`/`vary` 付与 → 本体圧縮）が
**そもそも書かれていなかった**。

設定は受理され、検証も通り、警告も出ない。**「設定したのに黙って効かない」**タイプの
不具合であり、単体・統合・E2E をすべて通過する。

## 影響

- HTTP/3 で静的ファイルを配信する全構成。転送量が圧縮前のままになる。
- 設定・README 上は圧縮されると読めるため、挙動とドキュメントが食い違っていた。

## 発見の経緯

F-169（静的配信の圧縮結果キャッシュ）の実装中、圧縮の呼び出し箇所を洗い出す過程で
判明した。`tools/perf` の `h3_file_compression` 構成は HTTP/3 行では圧縮が効かない
まま計測されていたことになる（HTTP/2 行の劣後は F-169 が扱う別要因）。

## 改修

`resolve_http3_compression_config`（パス設定 > HTTP/3 設定 > 既定、の優先順位。
`handle_proxy` が既に使っているもの）で圧縮設定を解決し、HTTP/2 の
`build_h2_compressed_file_response` と同じネゴシエーションを

- `handle_sendfile`（`SendFileRequest` に `compression` / `client_encoding` を追加）
- `Backend::File` / `Backend::MemoryFile` の分岐

へ適用する。`content-encoding` と `vary: Accept-Encoding` を付与し、非圧縮時は
`Bytes::from_owner` で `Arc<Vec<u8>>` の参照カウントクローンに留める
（ディープコピーを増やさない）。

`SendFile` 経路の圧縮結果は F-169 の圧縮結果キャッシュ（`cache::compressed`）に
載せる。`MemoryFile` はファイルシステムパスを持たずキャッシュキーを作れないため
毎回圧縮する（HTTP/2 側の `MemoryFile` 経路と同じ扱い）。

## 再発防止

「設定が黙って無視される」不具合は、ルート解決結果を `_` 束縛で捨てている箇所に
潜みやすい。`src/http3_server.rs` で `_route_*` のように破棄している値は、
**意図的な破棄なのか実装漏れなのかをコメントで明示する**こと。
