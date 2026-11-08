# B-349: 静的コンテンツキャッシュの本体と open_file_cache のエントリが違う版を持つと、ヘッダと本文の版が食い違う

**状態: 修正済み（feat/v0120-perf、2026-11-08）**

[B-348](B-348-fast-path-drops-open-file-cache-fd.md) と同じく、静的配信の 2 つのキャッシュの経路を読み直して見つけた。

## 事象

`open_file_cache`（メタデータと fd）と `static_file_cache`（本体）を両方有効にしたルートで、ファイルを差し替えた後、片方のキャッシュだけが
新しい版になっている間、応答のヘッダ（`Content-Length`・`ETag`・`Last-Modified`）と本文の版が食い違う。2 つのキャッシュは別々の時刻に
入って別々に期限が切れる（同じ `valid_duration_secs` でも）ので、既定の設定のままデプロイでファイルを替えるだけで起きる。

- HTTP/2・HTTP/3: 新しい版の検証子と古い本体を返す。`Content-Length` は新しい版の大きさで、DATA は古い本体の長さなので、クライアントは
  ストリームを不正な応答として捨てる（大きさが同じなら、古い本体を新しい `ETag` で返し、クライアントはそのまま保存する）。
- HTTP/1.1（Linux の平文・rustls の経路など、本体キャッシュを使う経路）: 古い本体を新しい大きさで切って返す（8 バイトを 2 バイトに
  替えると `Content-Length: 2` で `AA`）。どの版にも無い本文。
- 高速経路の無い OS（macOS・Windows・OpenBSD・NetBSD）の HTTP/2・HTTP/3: 本体をパスで読むので、エントリの古い大きさ・検証子で
  新しい中身を返す。

v0.6.0（F-150 で HTTP/1.1 に本体キャッシュを使い、B-65 で HTTP/2・HTTP/3 の 2 つのキャッシュを 1 回の offload にまとめた版）から。

## 原因

- 本体キャッシュは「パス → 本体」だけを持ち、どの版の本体かを持たない。引く側は open_file_cache のエントリ（ヘッダの元）と組み合わせる
  が、版を照合していなかった。
  - `get_static_file_with_content` の 1（エントリと本体キャッシュが両方当たれば offload 無しで返す）。
  - HTTP/1.1 の `load_file_bytes`（本体キャッシュに当たればエントリの大きさで切る。足りなければ fd から送り直すので、伸びた場合だけは
    正しかった）。
- `get_static_file_with_content` の 3（高速経路の無い OS・高速経路を使えないルート）は、エントリの fd でなくパスで本体を読んでいた。

## 対応

- 本体キャッシュのエントリに版（`cache::FileVersion` = 大きさと更新時刻。検証子の元と同じ）を持たせ、引くときに open_file_cache の
  エントリの版と照合する（`content_cache::get_cached(path, cfg, version)`。違えば外れとして扱い、読み直して同じ版で入れ直す）。
  当たったときに増えるのは比較 2 つだけ。
- 入れるときは、本体を読んだ fd と同じ版を付ける（高速経路は開いた fd の fstat、HTTP/1.1 はエントリの版でエントリの fd から読む）。
- 3 の経路は、エントリの fd からエントリの大きさだけ読む（offload の中で `pread`）。読み終えたら fd の `fstat` でファイルがその版のまま
  か確かめ、読めない・版が変わった（`cp` の上書きのように同じ inode のまま書き換えられた）ときはエントリを両方捨てて 1 回だけ開き直す。
  fd の無いエントリ（開けなかったとき）だけパスで読む。

## テスト

- 単体: `content_cache` の `insert_bytes_then_get_cached_round_trip` に、大きさ・更新時刻が違う版では外れることを足した。本体キャッシュに
  入っていないことを確かめる 4 件は、版を問わない `is_cached` で見る（版の照合で見落とさない）。
- 単体: `cache::static_file::tests::fallback_reads_the_entry_version_from_its_fd`（高速経路を使えないファイルで、`mv` の後も本体キャッシュから
  消えた本体をエントリの fd から同じ版で読む。同じ inode のままの上書きは開き直して新しい版を返す）。
- 自己完結型 E2E: `b348_b349_static_caches_answer_one_version_per_response` の `/b/`（open_file_cache 2 秒、本体キャッシュ 600 秒）。
  h2c で両方を温める → `mv` で替える → 2.5 秒待つ（open_file_cache だけ期限切れ）→ HTTP/1.1 と h2c が新しい版の本文・`Content-Length`・
  同じ `ETag` を返すこと。縮む（8 → 2 バイト）・伸びる（2 → 12 バイト）の両方。

結果（Linux x86_64、2026-11-08）: 修正後は単体と自己完結型 E2E が `full`・`full,epoll` とも通った。修正前（`96cbc81b`）で、上の E2E から
`/a/`（B-348）を除いて回すと、`/b/` の縮む方で HTTP/1.1 が `AA`（古い 8 バイトの本体を新しい 2 バイトで切ったもの）を返して落ちた。
ほかのプラットフォームは [platform_verification.md](../../perf/platform_verification.md)。
