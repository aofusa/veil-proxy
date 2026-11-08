# B-348: HTTP/2・HTTP/3 の静的配信が open_file_cache のエントリを fd の無いものに置き換え、HTTP/1.1 が差し替えの後のファイルを古い大きさで切って返す

**状態: 修正済み（feat/v0120-perf、2026-11-08）**

v0.12.0 の最終の検証で、FreeBSD aarch64 の自己完結型 E2E の `f272_static_cache_follows_disk_changes` が「HTTP/1.1 unwatched route」で
落ちて見つけた（監視しないルートは有効期限まで古い中身を返すはずが、HTTP/1.1 だけ `mv` で差し替えた後の中身を返す）。f272 は v0.11.0 で
足した試験で、BSD では自己完結型 E2E を回していなかった（[B-347](B-347-standalone-e2e-f271-stale-after-f293.md)）。Linux では HTTP/1.1 が
本体キャッシュから返すので、この試験では見えない。

## 事象

同じファイルを HTTP/1.1 と HTTP/2・HTTP/3 の両方で配信していて、`open_file_cache` の有効期限内にファイルを `mv` で差し替えると
（デプロイの普通の手順）、HTTP/1.1 の応答の本文が、ヘッダ（`Content-Length`・`ETag`・`Last-Modified`）の版と違うファイルから送られる。

- 新しいファイルの方が長いと、新しい中身の先頭を古い大きさで切った本文を返す（`AAAA` を `BBBBBBBB` に替えると `Content-Length: 4` で
  `BBBB`）。どの版にも無い本文で、ブラウザや CDN は古い版の `ETag` で保存する。
- 新しいファイルの方が短いと、`Content-Length` に届かないまま接続を閉じる。
- 同じ大きさなら、新しい中身を古い版の `ETag`・`Last-Modified` で返す（f272 で落ちたのはこれ）。

Linux と FreeBSD（高速経路のある OS）。v0.7.0 で HTTP/1.1 が open_file_cache の fd で送るようになって防いだはずの事象が、
HTTP/2・HTTP/3 と混ざると残っていた（それより前は HTTP/1.1 だけでも起きていた）。

## 原因

- HTTP/1.1 の静的配信は、open_file_cache のエントリが持つ fd で送る（`sendfile(2)`・kTLS・`pread`）。エントリを作るときに開いた fd
  なので、`mv` で差し替えられても有効期限まではエントリの版（大きさ・検証子）と同じ中身を送れる。エントリに fd が無ければ、
  配信のたびにパスで開き直す。
- HTTP/2・HTTP/3 の高速経路（`get_static_file_with_content` の 1 回の offload で open + fstat + read。B-65）は、読み終えた fd を閉じ、
  **fd の無いエントリ** で open_file_cache を置き換えていた（`CachedFileInfo::from_open_metadata` は `shared_file: None`）。
  本体キャッシュに当たらないとき（本体キャッシュの無いルートは毎回、あるルートも HTTP/1.1 が先にエントリを作った後の最初の 1 回。
  FreeBSD の平文の HTTP/1.1 は本体キャッシュに入れない）に高速経路を通り、そのたびにエントリから fd が落ちる。
- fd の無いエントリで HTTP/1.1 はパスで開き直すので、差し替えの後は新しいファイルをエントリの古い大きさで送る。
- 性能の面でも、HTTP/2・HTTP/3 と混ざると HTTP/1.1 が要求ごとに `open`・`close` するようになり、fd を共有する意味が消えていた。

## 対応

- 高速経路で open_file_cache へ入れるエントリにも fd を持たせる（`finish_fast` の `keep_fd`。共有のキャッシュへ入れないルート
  （B-321）と open_file_cache の無いルートでは持たせない）。読み取りは全経路がオフセット指定なので、読んで位置が進んだ fd も共有できる。
- 前のエントリが同じ版（大きさと更新時刻）の fd を持っていれば、それを引き継いで開いた fd を offload の中で閉じる
  （`ReusableFileInfo::fd_of_same_version`）。毎回新しい fd に替えると、前の fd を閉じる `close(2)` がワーカーで走る。
- 版が変わっていれば新しい fd に替える（エントリの大きさ・検証子と fd が常に同じ版）。

## テスト

- 単体: `cache::static_file::tests::fast_path_keeps_an_fd_in_the_open_file_cache_entry`（高速経路が先・HTTP/1.1 が先の両方で
  エントリが fd を持ち、同じ版なら同じ fd を引き継ぐ。`mv` の後も有効期限内のエントリの fd は古い版を読み、高速経路の後は新しい版）。
- 自己完結型 E2E: `b348_b349_static_caches_answer_one_version_per_response` の `/a/`（本体キャッシュ無し、open_file_cache 600 秒）。
  HTTP/1.1 → h2c ×2 → `mv` で 4 バイトを 8 バイトに替える → HTTP/1.1 がエントリの版（`AAAA`、`Content-Length: 4`）を返すこと。
  修正前は `BBBB` を返して落ちる。

結果（Linux x86_64、2026-11-08）: 修正後は単体（上の試験を含む）と自己完結型 E2E の `b348_b349_static_caches_answer_one_version_per_response`
が `full`・`full,epoll` とも通った。同じ試験を修正前（`96cbc81b`）で回すと、`/a/` の HTTP/1.1 が `Content-Length: 4` で `BBBB`（差し替えた後の
8 バイトの先頭）を返して落ちた。ほかのプラットフォーム（FreeBSD では f272 も）は [platform_verification.md](../../perf/platform_verification.md)。
