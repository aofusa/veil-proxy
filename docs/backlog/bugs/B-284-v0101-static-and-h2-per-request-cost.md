# B-284: v0.10.1 の候補で静的配信と HTTP/2 の要求ごとの費用が増え、v0.10.0 より遅い

**状態: 修正済み（v0.10.1）**

## 出典

v0.10.1 の公開前の性能の確認（`tools/perf/suite_ab.sh`、aofish の Docker、v0.10.0 との交互 A/B）。

## 事象

v0.10.0 と比べて、静的配信の構成が HTTP/1.1・HTTP/2・HTTP/3 とも遅かった（5 回の交互 A/B の中央値）。

| 構成 | HTTP/1.1 | HTTP/2 | HTTP/3 |
|---|---|---|---|
| `h2_1_ktls_0_lb_kernel_ofc_0`（open_file_cache 無し） | 0.80 | 0.89 | — |
| `h2_1_ktls_0_lb_kernel_ofc_1` | 1.00 | 0.88 | — |
| `h3_file_cache`（`[route.cache]`、open_file_cache 無し） | 0.95 | 0.91 | 0.84 |
| `h2_1_feat_metrics` | 0.93 | 0.85 | — |

## 原因

B-266・F-287（静的配信の `ETag`・`Last-Modified`・Range・304 をプロトコル間でそろえる）で増えた要求ごとの仕事。

1. **検証子を要求ごとに組み立てていた。** open_file_cache の無いルートはファイル情報を要求ごとに offload で作る。そこで
   `Validators`（`ETag`・弱い `ETag`・`Last-Modified`・`content-type`・長さ）を値ごとの `String`・`format!`（7 回の確保）で作っていた。
   offload のスレッドが律速になり、HTTP/1.1 はサーバの CPU 使用率が下がったまま 2 割遅くなった。
2. **本体だけ読み直す経路でファイル情報を 2 回作っていた。** open_file_cache だけ（本体キャッシュ無し）のルートは要求ごとに
   `open_and_read_fast` で開き直す。`finish_fast` と `read_into_bytes` がそれぞれ `CachedFileInfo::from_open_metadata` を呼び、MIME の推測と
   検証子の組み立てが 2 回ずつ走っていた（MIME の 2 回は以前から）。キャッシュに前のエントリがあっても作り直していた。
3. **HTTP/2 のヘッダが倍になった分の HPACK の静的表の線形探索。** 静的配信の応答に `accept-ranges`・`etag`・`last-modified`・
   `content-length` が増え、ヘッダごとに静的表の 61 件を順に比べていた。
4. **HTTP/2 の受信の見回り（B-274）のタイマー。** 処理中のストリームがある接続は待つたびに 1 秒のタイマーを持っていた（本文の無い要求だけでも）。

## 修正（v0.10.1）

- `Validators::build` は値をすべて 1 つのバッファに書いて `slice` で切り分ける（確保はバッファ・`Bytes` の共有部分・`Arc` だけ）。
  `Validators::shared_for_file` はスレッドごとの小さな表（直接写像 64 件、大きさ・更新時刻・型が一致したときだけ使う）で同じ検証子を
  使い回す。MIME の型の文字列は `static_response::intern_mime` で共有する（`src/static_response.rs`、`src/cache/`）。
- `finish_fast` はファイル情報を 1 回だけ作り、前のエントリがあれば MIME をそのまま、大きさと更新時刻が同じなら検証子もそのまま
  引き継ぐ（`file_cache::ReusableFileInfo`）。
- HPACK の静的表の検索は名前の `match` で範囲を決めてから値を比べる（`src/http2/hpack/table.rs`。全エントリの線形探索と一致することを
  単体試験で確かめる）。
- HTTP/2 の受信の見回りのタイマーは、クライアントがまだ送っているストリームがあるときだけ張る（`StreamManager::has_receiving`）。
- HTTP/1.1 の静的配信のヘッダ名は決まった表記を写す（1 バイトずつ大文字にしない）。`Etag` は RFC 9110 の表記 `ETag` になる。

## 確認

- 単体試験: `reread_reuses_mime_and_validators_until_file_changes`・`shared_validators_and_mime_are_reused_per_thread`・
  `test_static_table_find_matches_linear_scan`・`has_receiving_ignores_requests_without_body`、検証子の各値の切り分け。
- 要求あたりの命令数（aofish、`perf stat`、4 回の交互の中央値。スループットはホストの揺れが ±15% あったため命令数で比べた）:

  | 構成 | HTTP/1.1 | HTTP/2 |
  |---|---|---|
  | `h2_1_ktls_0_lb_kernel_ofc_0` | 1.00 | 0.96 |
  | `h2_1_ktls_0_lb_kernel_ofc_1` | 1.03 | 0.92 |

  サーバの要求あたりの費用は v0.10.0 と同じかそれ以下。残る差（`ofc_1` の HTTP/1.1 の +3%）は v0.10.1 で応答に増えたヘッダ
  （`ETag`・`Last-Modified`・`Server`。nginx も付ける）を書く分。スループットの比較は修正後の `suite_ab.sh` の結果をリリースの記録に残す。

## 関連

- [B-266](B-266-head-local-responses-send-body.md)、[F-287](../features/F-287-static-etag-conditional-requests.md)、[B-274](B-274-route-client-timeouts-h1-only.md)
