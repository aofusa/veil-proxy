# B-244: HTTP/1.1 の上流が chunked の応答を途中で止めると、応答の途中の上流接続がプールに戻り、別のクライアントの要求に前の応答の続きが混ざる

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー 2 回目（F-281 の続き）の FW-01。aofish の Docker Desktop で v0.9.2（`veil:v092-base`）を使って **実機再現**
（`docs/artifacts/security/repro/`、git 管理外。分析は `docs/artifacts/security/analysis/07-f281-followup.md`）。

## 事象

HTTP/1.1 のリスナー（平文・rustls とも）から HTTP/1.1 の上流へ中継し、上流の応答が `Transfer-Encoding: chunked` で、
チャンクの間が `READ_TIMEOUT`（30 秒。`src/pool.rs`）を超えて空く（SSE・long-poll・遅い生成）と:

1. `transfer_response_body_with_cache`（`src/proxy/compress.rs` L854〜）・`transfer_https_response_body`（`src/proxy/https.rs`）の chunked の腕は
   `timeout` の `Err` で **黙って `break`** し、戻り値は転送量だけ（`transfer.rs` の `ChunkedTransferResult` に相当する情報を捨てる）。
2. 呼び出し側（`compress.rs` L423〜L456、`https.rs` L542〜L570）は `content_length.is_some()` のときだけ不完全を検出する（B-17）。chunked では
   `backend_wants_keep_alive` のまま `(total, status, true, false)` を返す。
3. `forward.rs` L1034〜L1041 が **応答の途中の上流接続を `HTTP_POOL` に戻し**、クライアント A の接続も閉じない（A は続きを待ち続ける）。
4. 同じワーカーで同じ上流へ向かう **別のクライアント B** の要求がその接続に書かれ、上流は A の応答の続き（tail チャンク）を送ってくる。
   B-93 の `MSG_PEEK` は取り出し時点で未読データが無ければ通る。

再現（`repro/fw-run.log`）: A が `GET /slowchunk`（最初のチャンクの後 40 秒停止）、33 秒後に B が新しいクライアント接続で `GET /id`。
上流のログは同じ接続 2 に `GET /id` が tail の後に来たことを示し、veil は tail を B の応答ヘッダとして読めず
`Backend response header read timeout` → **B は 504**。A は 50 秒待っても tail を受け取れない。

上流の続きが偶然 `HTTP/1.1 200` で始まる形（複数の応答が残る・tail がヘッダに見える）なら **B が A の応答データを受け取る**（クロスユーザの取り違え）。

## 影響

- **P1（セキュリティ）**: 攻撃者は A 役として遅いエンドポイントに要求を出すだけで、同じ上流を使う他の利用者の要求を 504 にできる（接続のポイズニング）。
  上流の応答の形によっては他人の応答が自分の要求に混ざる。要求側の条件はリモートから作れる（上流に 30 秒以上止まるエンドポイントが 1 つあればよい）。
- kTLS の `src/proxy/splice.rs`（L330〜450、タイムアウトで `keep_alive=false`）、上流 HTTP/2（`h2.rs` L2790〜、`reusable=false`）、HTTP/3（B-104 の
  「ちょうど読み切った接続だけ戻す」）は正しい。影響は HTTP/1.1 の平文・rustls の経路に限られる。

## 改修案

- `transfer_response_body_with_cache`・`transfer_https_response_body`・`transfer_compressed_response` の chunked の腕が **完了したか** を返す
  （`transfer.rs` の `ChunkedTransferResult` を使う）。`Complete` 以外（タイムアウト・EOF・不正・上限超過）は `backend_wants_keep_alive=false` と
  `client_must_close=true` を返し、`forward.rs` はプールに戻さずクライアントを閉じる（B-17 の CL 不足と同じ扱い）。
- `Content-Length` 無し・chunked 無し（EOF 終端）の応答も同じ（EOF まで読み切れていなければ戻さない）。
- 戻す直前の最終防衛として、`HTTP_POOL::put` の前に「応答の終端を読み切った」ことを呼び出し側が明示する型（`CompleteResponse` のような証拠）を要求し、
  経路ごとの見落としを型で防ぐ。

## 受け入れ条件

- E2E: 上流が chunked の途中で `READ_TIMEOUT` を超えて止まったとき、(a) クライアント A の接続が閉じられ、(b) 続く B の要求が **新しい** 上流接続で届く
  （上流の接続 ID を応答ヘッダで返す試験用サーバ）。平文・rustls・kTLS・HTTP/2 上流・HTTP/3 で同じ結果。
- `tools/container_security` の `smuggling` フェーズに上流側のケースを足す（F-279）。

## 修正（v0.10.0）

- 上流の HTTP/1.1 応答の本文の区切りを `ParsedResponse::body(is_head)`（`http_utils::ResponseBody`: None / Length / Chunked / UntilClose）で一度だけ決め、全経路で同じ転送ヘルパー（`proxy/transfer.rs` の `response_body_start`・`transfer_response_body_rest`・`collect_response_body(_raw)`）を使う。結果（`ResponseBodyOutcome`）は区切りどおりに終わったか・終端の後ろにバイトがあったかを返し、**区切りどおりに読み切って余りの無い接続だけ** をプールへ戻す。途中で止まった応答はクライアント接続を閉じる（HTTP/2・HTTP/3 はストリームのリセット）。
- 対象: 平文・rustls（`compress.rs`・`https.rs`）、kTLS（`splice.rs`。タイムアウトで keep-alive を落としていたのに加え、終端の後ろのバイトを送っていた）、HTTP/2 で受けた要求（`h2.rs` の `h2_relay_backend_response`。長さ不明の応答で最初の読み込みしか送らず、途中で止まった本文も送っていた → 502）、`buffering = full`（`buffered.rs`。途中の本文を完全な本文として送り・キャッシュしていた → 502、上限超過で切り詰めた本文を元の Content-Length で送っていた → 507）、キャッシュのバックグラウンド更新（`server.rs`。途中の本文・chunked の最初の読み込みだけを保存していた → 読み切った Length・Chunked だけを保存）。
- 同時に直したもの: 圧縮する chunked 応答がチャンクの枠ごと圧縮されていた、途中の本文が完全な応答としてキャッシュ・圧縮されていた、終端の後ろのバイトがクライアントへ送られていた。
- 試験: E2E `test_b244_stalled_chunked_upstream_is_not_pooled`（tests/test_backends の framing サーバーが 35 秒止めた接続に別の要求が届いたかを返す。HTTP/1.1・HTTP/2・`buffering = full` を同時に止め、新しいクライアントから 16 本要求する）。修正前のコードで失敗することを確認。kTLS の経路は E2E の既定で kTLS が無効なので同じヘルパーの単体試験と同じ判定で担保。

## 関連

- [B-17](B-17-malformed-backend-client-hang.md)（CL 不足の切断）、[B-93](B-93-pooled-upstream-closed-while-idle.md)（取り出し時の生存確認）、
  [B-104](B-104-http3-upstream-no-keepalive.md)（HTTP/3 の「読み切った接続だけ戻す」）、[B-245](B-245-h1-head-204-304-upstream-body-wait.md)、
  [B-246](B-246-upstream-response-cl-te-precedence.md)、[F-281](../features/F-281-security-review-remaining-areas.md)
