# B-304: 圧縮した静的配信の 304 が強い ETag を返し、方式間で ETag が同じ。圧縮結果のキャッシュがファイルの差し替えを見ない

**状態: 未修正（2026-11-05 起票。コードレビューで発見。行番号は f8dc80de）**

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

## 関連

- [F-287](../features/F-287-static-etag-conditional-requests.md)、[F-294](../features/F-294-precompressed-static-siblings.md)
