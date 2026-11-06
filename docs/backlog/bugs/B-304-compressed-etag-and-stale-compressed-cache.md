# B-304: 圧縮した静的配信の 304 が強い ETag を返し、方式間で ETag が同じ。圧縮結果のキャッシュがファイルの差し替えを見ない

**状態: 修正済み（v0.11.0）**

## 事象（コードの読み取り）

1. `static_response.rs:296-306,478-484`: 圧縮した 200 は弱い `W/"m-s"` を付けるが、条件付き要求は表現を決める前に評価するので 304 は強い `"m-s"` を返す
   （RFC 9110 §15.4.5 の「304 は 200 で送ったはずの ETag」に反する）。gzip・br・zstd が同じ弱い ETag を共有する（共有キャッシュが方式を取り違え得る）。
2. 圧縮結果のキャッシュ（F-169）のキー `(path, enc, level)` にファイルの同一性が無い。`max_file_size_bytes`（1MiB）を超え 8MiB 以下のファイルを差し替えると、
   TTL が切れるまで新しい ETag で古い圧縮本文を返す。
3. （Low）416 に `Vary` が付かない。HTTP/1.1 の HEAD が Content-Length のために 8MiB までのファイルを読んで圧縮する。圧縮に失敗すると圧縮のヘッダのまま元の本文を送る。

## 改修案

1. 表現を先に決めてから、その表現の検証子で条件付き要求を評価する（[F-294](../features/F-294-precompressed-static-siblings.md) の `Repr` と同じ構造。こちらを先に入れる）。
   方式ごとに ETag を変える（`W/"m-s-br"`）。
2. `CachedVariant` に size・mtime を持たせ、ヒット時に比べる。
3. 416 にも `Vary`。HEAD は圧縮後の長さをキャッシュから得られないときは `Content-Length` を付けない（または圧縮しない表現を返す）。圧縮の失敗は identity のヘッダで送る。
- 試験: 圧縮の 200 → 304 の ETag が同じ、方式ごとに ETag が違う、ファイルを差し替えると新しい本文。

## 修正

1. `plan()` は表現（圧縮方式）を先に決め、その表現の ETag で `If-None-Match` を評価する。`Plan::NotModified { encoding }` で 304 も 200 と同じ ETag。方式ごとの弱い ETag（`W/"…-zstd"`・`-br`・`-gzip`・`-deflate`）を `Validators` に 1 回だけ作る。
2. 圧縮結果のキャッシュ（`cache::compressed`）はファイルの同一性（`file_ident(size, mtime)`）を持ち、ヒット時に比べる（差し替えたファイルに古い圧縮本文を返さない）。
3. 416 にも `Vary`。HTTP/1.1 の HEAD は圧縮する表現でもファイルを読んで圧縮せず、`Content-Length` を付けない。メモリへの圧縮は失敗しない（`Vec` への書き込み・ロード時に検証したレベル）ので、失敗時の元の本文への退避はそのまま残した。
- 試験: `conditional_requests_use_the_representation_etag`・`changed_file_identity_misses`。

## 関連

- [F-287](../features/F-287-static-etag-conditional-requests.md)、[F-294](../features/F-294-precompressed-static-siblings.md)
