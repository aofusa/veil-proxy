# B-160: File のルートを相対パスで書くと、Linux の HTTP/1.1 の静的配信が全件 403 になる

**状態: 完了（feat/distribution-tickets、2026-10-23。F-247 の作業で発見）**

## 現象

`[route.action] type = "File"` の `path` を相対パス（例: `path = "examples/www/"`）で書くと、Linux の HTTP/1.1 では
存在するファイルがすべて 403 になる。h2c・HTTP/2 は 200 を返す。

- **再現:** リリースのアーカイブの例（F-247）を `./veil -c examples/config.toml` で起動する。
  - `curl http://127.0.0.1:8080/`: 403
  - `curl --http2-prior-knowledge http://127.0.0.1:8080/`: 200
- Landlock の有無に依らない。絶対パスで書けば 200。
- 文書・`examples/config.toml` の例には `path = "./www/static/"` のような相対パスがあるので、例のとおりに書くと起きる。

## 原因

- **開け方:** Linux の静的配信は、登録した静的ルートの dirfd から openat2（`RESOLVE_BENEATH`。F-153）で開く。
  そのとき `file_info.canonical_path` は生パス（`base_path` の join。相対のまま）になる。
- **包含の判定:** HTTP/1.1 の判定（`cache::sendfile_base_contains`）は、そのパスを canonical 形（絶対パス）の
  `canonical_base` と比べていた。相対の生パスは決して一致しないので、403 になっていた。
  - 生の `base_path` との比較は FreeBSD（capsicum の dirfd 経路。F-123）でしか行っていなかった。

## 修正

- 生の `base_path` 配下も許可する比較を、全 OS に広げた。
  - **安全性:** dirfd 経路の生パスは、カーネルが封じ込めを保証している。canonicalize の経路のパスは
    シンボリックリンクと `..` を解決済みなので、生の `base_path` を前方一致で含むのはルートの中だけ（FreeBSD と同じ理由）。
  - **ホットパス:** 先に canonical 形と比べ、外れたときだけ生パスと比べる。通常の絶対パスの設定では、比較は 1 回のまま。
- 単体試験: 相対のルートの配下は許可し、ルート外・前方一致だけの別ディレクトリ（`examples/wwwx`）は拒む。
- 確かめたこと: アーカイブの例で HTTP/1.1・h2c とも `/`・`/quickstart.html` が 200、無いファイル・`/../etc/passwd` は 404。

## 関連

- [F-153](../features/F-153-static-open-resolve-beneath.md)
- [F-247](../features/F-247-distribution-foundation.md)
