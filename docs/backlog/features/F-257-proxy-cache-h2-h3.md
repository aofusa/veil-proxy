# F-257: HTTP/2・HTTP/3 のクライアントと、HTTPS の上流にもプロキシのキャッシュを効かせる

**状態: 完了（feat/protocol-parity。2026-10-23 起票、2026-11-22 対応。B-162 の調査で分かった）**

## 起票時の状態

- `[route.cache]` は HTTP/1.1 のクライアントの要求（`proxy::forward` の経路）だけで読み書きされる。
- HTTP/2（`proxy::h2`）・HTTP/3（`http3_server`）の中継はキャッシュを見ずに上流へ送る。
- HTTPS の上流（`proxy::https::proxy_https_pooled`）は、キャッシュを引くが保存しない（`proxy::forward` のコメント
  「HTTPS は別途実装が必要」）。E2E の `/cached/*` の上流は HTTPS なので、そこではキャッシュから返らない。
- 文書には v0.9.0 で書いた（`compression-cache.md` の Notes）。

## 対応（2026-11-22）

- **引き当ての共有**（`proxy::cache_lookup`）: キーの組み立て・当たり外れ・304・当たりの応答のヘッダ（`X-Cache`・
  `remove_response_headers`・`add_response_headers`・自分の `server`）を 3 プロトコルで 1 つにした。要求ヘッダは
  `(名前, 値)` の組の反復子（HTTP/1.1 の `header_pairs`・HTTP/2 の `h2_header_pairs`・HTTP/3 の `h3_header_pairs`）で渡し、
  要求ごとに `Vec` を作らない。引き当ては上流を選ぶ前（当たりで負荷分散の順番を進めない。B-481 と同じ順）。
  ホスト・要求の対象はバイト列で渡し、キャッシュのあるルートでだけ UTF-8 として読む（HTTP/3 のバッファ経路は
  `handle_request` が読んだ `:authority` を受け取り、ヘッダを探し直さない）。UTF-8 でない `Host`・`:authority` の要求は
  キャッシュを使わない（以前の HTTP/1.1 は `unknown` のホストのキーで保存していた）。
- **HTTP/2**: 引き当ては `h2_proxy` の入口。キャッシュのあるルートの、キャッシュし得るメソッドの要求は溜める経路
  （`h2_route_streaming_plan`）。外れは上流の種類（HTTP/1.1・HTTPS・h2c・HTTPS の h2）ごとに応答を取り込んで保存する。
  当たりは `h2_emit_cache_hit`（メモリの本文は `Bytes` をそのまま、ディスクの本文は offload で読む）。
- **HTTP/3**: 引き当ては `classify`（セキュリティの検査の後、上流を選ぶ前）。当たりはバッファ経路のタスクへ渡し
  （引き直さない・セキュリティの検査を二重にしない）、外れは流す経路（`BackendTaskParams.cache`）・溜める経路
  （`handle_proxy`）の両方で取り込んで保存する。
- **HTTPS の上流**（HTTP/1.1 のクライアント）: 平文の上流と同じく取り込んで保存する。HTTP/2 を選ぶ HTTPS の上流・h2c の
  上流（`proxy_h1_via_https_h2`・`proxy_h2c`）も保存する。
- **保存する応答の規則**（3 プロトコル共通）: veil が書き換えずに中継した応答だけ。圧縮して返す応答・Wasm が本文を受ける
  応答（当たりは Wasm を通らないので、書き換える前の本文を返すことになる）・トレーラのある応答・上流が閉じて終わる応答は
  保存しない。保存は `normalize_for_store` で整える（ホップバイホップを除き、chunked の本文を復号し `Content-Length` を
  付け直す。どのプロトコルの当たりも同じ本文を返す）。
- **裏の取り直し**（stale-while-revalidate）: `spawn_background_revalidation` をヘッダの反復子で受けるようにし、HTTP/2・
  HTTP/3 の要求から呼ぶ（疑似ヘッダは送らない）。TLS の上流にも接続する。
- 作業の途中で見つけた不具合: [B-491](../bugs/B-491-proxy-cache-ignores-route-store-settings.md)（保存がルートの設定を
  見ない・ディスクへの同期の書き込み）、[B-492](../bugs/B-492-proxy-cache-stale-never-served.md)（stale が一度も効かない・
  HTTP/1.1 の応答 2 つ）、[B-493](../bugs/B-493-h2-relay-duplicate-server-header.md)（HTTP/2 の `server` が 2 つ）、
  [B-494](../bugs/B-494-proxy-cache-storage-settings-not-applied.md)（保存先の設定が効かない。未着手）。
- **要求ごとのフューチャの大きさ**（`-Zprint-type-sizes` で対応前と比べた。キャッシュの無いルートの要求も同じフューチャを
  使うので増やさない）: 当たりを返す処理・stale-if-error を返す処理は当たりを値で受ける別の `async fn`（`h2_serve_hit`・
  `serve_hit`）か箱に入れ、本文の取り込みは値で受けて返す（`cache_lookup::capture`。`&chunk` で借りてから送り先へ移すと、
  借りた局所変数が移した後もフューチャに残る）。HTTP/3 のバッファ経路の要求タスクは `async move` のブロックを返す関数に
  した（`async fn` は引数を 2 度置く）。HTTP/2 のストリームのフューチャは 12,360 → 12,448 バイト（増えたのは上流の種類ごとの
  関数へ渡す保存の文脈の引数）、HTTP/3 のバッファ経路は 6,560 → 6,528、流す経路は 5,624 → 5,656、HTTP/1.1 の
  `handle_proxy` は 12,016 → 11,600。
- 文書: `compression-cache.md`（英日。全プロトコル・保存する応答の規則・stale の期間・保存先の制限）、
  `observability.md`（英日。キャッシュの系列も全プロトコルで動く）、`features.md`・`migration-nginx.md`・
  `migration-envoy.md`（英日）、`examples/config.toml`。

## 試験

- 自己完結型 E2E: `f257_proxy_cache_on_every_protocol`（HTTP/1.1・h2c・HTTP/2（TLS）・HTTP/3 で、上流が HTTP/1.1・h2c の
  どちらでも 2 回目は `X-Cache: HIT` で上流へ 1 回だけ。同じ URL はどのプロトコルで保存してもほかのプロトコルへ返す。
  chunked の応答は復号して保存する）、`f257_cache_skips_non_utf8_host`（UTF-8 でない `Host`・`:authority` の要求は
  2 回とも上流へ）、`feature_protocol_matrix` の「proxy cache」（4 プロトコルとも上流へ 1 回）。
- E2E: `test_e2e_cached_https_upstream_hits`（HTTPS の上流（HTTP/1.1・ALPN の h2）の 2 回目が `X-Cache: HIT`）。
- 単体: `cache_lookup` のキー・当たりのヘッダ、`normalize_for_store`、B-491・B-492 の試験。
- 性能（キャッシュの無いルートが遅くならないか）: 対応前のコミットとの交互 A/B（キャッシュの無いプロキシのルート、上流は
  HTTP/1.1、クライアントは HTTP/1.1・h2c、3B・54KB、8 ラウンドの組ごとの差の中央値）で rps −1.8〜−0.8%・要求あたりの CPU
  −1.5〜+0.8%。同じ組み合わせを取り直すと HTTP/1.1 の 3B は +1.9% から −1.8% へ符号が入れ替わり、ラウンドごとの差は ±3〜5%
  揺れる（ほかの作業が負荷 4〜7 で走る共有の機械）ので、差は揺れの内側。HTTP/3 はこの機械に負荷ツールが無く測っていない。

## 関連

- [B-162](../bugs/B-162-proxy-cache-small-responses-never-stored.md)
