# B-269: HTTP/1.1 の静的配信（`type = "File"`・`mode = "memory"`）に `[route.compression]` が効かない

**状態: 未着手（2026-11-04 起票）**

## 出典

F-269 の HttpArena の validate で `static-tls compression` が SKIP になっていた（veil が HTTP/1.1 のファイルを無圧縮で返す）。
v0.10.0 の公開後に「バグか実装漏れか」を確かめた。v0.10.0 のソースのデバッグビルドに `[route.compression] enabled = true` の File・memory の
ルートを置き、`Accept-Encoding: gzip` で 4.8KB のテキストを要求した。

## 事象

| クライアント | `type = "File"` | `mode = "memory"` |
|---|---|---|
| HTTP/1.1（平文・TLS） | **無圧縮**（4800 バイト、`Content-Encoding` 無し） | **無圧縮** |
| h2c・HTTP/2（TLS） | gzip（59 バイト） | gzip |
| HTTP/3 | 圧縮する（コード。F-169） | 圧縮する |

- `h1::handle_requests` は `l7::resolve_route` が返す圧縮の設定を `_route_compression` として捨て、`handle_backend` の `SendFile`・`MemoryFile` の
  分岐は圧縮を見ない（`handle_sendfile` は `sendfile(2)`/`splice` で送る）。
- HTTP/2・HTTP/3 は `build_h2_compressed_file_response`・`compress_body_h3` で圧縮し、`static_file_cache` があれば圧縮結果を
  `cache::compressed` に持つ（F-169）。HTTP/3 も以前は同じ漏れがあり、F-169 で直した。HTTP/1.1 だけ残った。

## バグか実装漏れか

実装漏れ（バグ）として扱う。

- 設定の説明（`compression-cache.md`）は File のルートに `[route.compression]` を書く例（「Static assets (high compression)」）を載せ、
  プロトコルの制限を書いていない。設定は 1 つで、3 つのプロトコルのうち 1 つだけ効かない。
- 一方、ライブラリの文書（`library-kit.md`・`library-production.md`）と B-174 は「HTTP/1.1 はファイルをそのまま送る」と制限として書いている。
  kTLS・`sendfile` のゼロコピーを守るために意図して外したのではなく、HTTP/2・HTTP/3 で圧縮を足したとき（F-169）に HTTP/1.1 を足さなかった。
  圧縮を有効にしないルートは今のまま `sendfile` で送れるので、両立できる。

## 影響

- P2（HTTP/1.1 で受ける静的配信で `[route.compression]` を有効にしたルート）: 転送量が減らない。HttpArena の `static-tls compression` が SKIP になる。
  回避策は無い（HTTP/2 か HTTP/3 で受けるしかない）。

## 修正方針

- `handle_backend` の `SendFile`・`MemoryFile` に圧縮の設定と `Accept-Encoding` を渡す。`should_compress` が `Some` のときだけ
  本文を読んで圧縮して送り、`None` なら今のまま `sendfile`/`splice`/kTLS で送る（圧縮しないルートの性能は変えない）。
- 圧縮結果は HTTP/2・HTTP/3 と同じ `cache::compressed::get_or_compress` で持つ（`static_file_cache` が有効なとき。キーは
  パス・エンコーディング・レベル）。本文は `static_file_cache` の `Bytes` を使い、要求ごとにファイルを読まない。
- `Vary: Accept-Encoding` を付ける。圧縮して返すときは Range を見ず 200 で全体を返す（nginx の gzip と同じ。`Accept-Ranges` も付けない）。
- HEAD は [B-266](B-266-head-local-responses-send-body.md) に合わせて本文を送らない（圧縮後の長さを返すかどうかを決める）。
- 文書: `library-kit.md`・`library-production.md`・`compression-cache.md` の「HTTP/1.1 はそのまま送る」を消す（英日）。
- 試験: E2E で HTTP/1.1（平文・TLS・kTLS）の File・memory の gzip・br・zstd、圧縮しない型（画像）と `min_size` 未満はそのまま、
  圧縮の無いルートは `sendfile` のまま（strace か `alloc_ab.sh` で確かめる）。性能は 54KB 静的 + zstd を nginx の `gzip_static` 無しの
  `gzip on` と交互 A/B。

## 関連

- [F-169](../features/F-169-static-compressed-variant-cache.md)、[B-174](B-174-precompressed-static-files-docs.md)、[F-269](../features/F-269-benchmark-submissions.md)、
  [B-270](B-270-h2c-upstream-responses-not-compressed.md)（h2c の上流の応答の圧縮）、[F-286](../features/F-286-range-requests-h2-h3-static.md)（Range）
