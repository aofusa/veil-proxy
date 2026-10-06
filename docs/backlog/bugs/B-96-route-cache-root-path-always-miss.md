# B-96: `path = "/"` のルートでルートキャッシュが一度も効かず、毎リクエストがフル探索になっていた

## 事象

FreeBSD aarch64 の HTTP/3 CPU プロファイル（3B 静的）で、キャッシュがヒットしているはずの
定常状態なのに `lru::LruCache::put`・`from_utf8_lossy`（`Utf8Chunks`）・SipHash が
リクエストごとに出ていた。

## 原因

`find_backend_unified` はルートキャッシュ（キー = host/path/method/送信元 IP の xxh3）に
ヒットすると、ハッシュ衝突対策として `matches_conditions` で条件を再検証する。その中の
`matches_path_pattern("/", "/small.html")` は、`"/"` を取り除いた残り `"small.html"` が `/` で
始まらないため **false** を返していた。一方ルーター本体（`routing::PathRouter::add_route`）は
`path` 未指定と `"/"` を `any_path`（全パスに一致）として扱う。

その結果、最も一般的な catch-all ルート（`path = "/"`）では**ヒット検証が毎回失敗**し、
全リクエストが「`String::from_utf8_lossy` ×2 + 候補列挙 + 条件評価 + LRU の put
（SipHash）」のフル探索に落ちていた。HTTP/1.1・HTTP/2・HTTP/3 のすべてに影響する
（HTTP/3 は `classify` と `handle_request_impl` で 1 リクエスト 2 回探索するので 2 倍）。

## 修正

- `matches_path_pattern` で `"/"` を全パス一致にする（ルーター本体と同じ意味論）。
- あわせて、ルートキャッシュの LRU と HTTP/3 の接続マップのハッシャを既定の SipHash から
  シード付き xxh3 にした（LRU のキーは既に xxh3 済みの 64 ビット値、接続マップのキーは
  B-94 の HMAC 出力で、どちらもデータグラム / リクエストごとに引く）。シードは
  プロセス（LRU はスレッド）ごとに変えるので、攻撃者が衝突キーを量産してバケットを
  偏らせる HashDoS には耐性を保つ。

ヒット検証自体（衝突したキーで別ルートを適用させない防御）は残している。キャッシュキーは
固定シードの 64 ビット xxh3 なので、検証を外すとオフライン探索した衝突パスでルートの
アクセス制御を迂回され得る。

## テスト

`upstream::tests::test_b96_root_path_pattern_matches_every_path`。

## 結果（FreeBSD 14.3 aarch64、3B、base / new 交互 3 ラウンドの対 nginx 比の中央値）

| | base | new |
|---|---|---|
| h3_file | 0.894 | **1.050** |
| h1_file_tls | 0.942 | **1.020** |
| h2_file_tls | 1.279 | 1.194（ノイズ圏） |
