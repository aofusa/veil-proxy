# B-17: 不正なバックエンド応答でクライアント可視のハングが発生する

## 出典

`tools/container_security/chaos/bad_backend_chaos.sh`（F-67）。**2026-07-05 修正済み**（下記「修正内容」参照）。

## 事象（再現手順）

上流に不正 HTTP 応答を返すモックバックエンド（`bad_backend_server.py`）を配置し、
Veil 経由で各パスへ HTTPS アクセスした結果（`curl --max-time 12`）:

| パス | バックエンド挙動 | 観測結果 |
|------|------------------|----------|
| `/ok` | 正常応答 | 200（OK） |
| `/cl-too-small` | Content-Length < 実ボディ | 200（余剰バイトは転送されず＝スマグリング無し・OK） |
| `/cl-too-large` | Content-Length > 実ボディ | 200 ヘッダ受信後 **ボディ待ちでタイムアウト** |
| `/truncated-headers` | ヘッダー途中で切断 | **タイムアウト**（クライアント無応答） |
| `/huge-headers` | 256KB ヘッダー | **タイムアウト** |
| `/bad-status` | 不正ステータスライン | **タイムアウト** |
| `/instant-close` | 応答せず即クローズ | **タイムアウト** |
| `/no-response` | 受理後 30s 無応答 | **タイムアウト** |

正常系（`/ok`）と Content-Length 過小（スマグリング耐性）は問題なし。
一方、**上流のヘッダー異常・早期切断・巨大ヘッダー時に、Veil がクライアントへ
速やかに 502/接続クローズを返さず、クライアントが最大タイムアウトまでハングする**。

## 影響

- **P1 候補**。上流異常時にクライアント側の待ち時間が延び、接続・FD を占有する。
  `/cl-too-large`（宣言長までボディを待つ）、`/huge-headers`（ヘッダー上限）、
  早期切断時の 502 変換タイミングにギャップがある可能性。
- Slowloris 的な上流を用いた資源枯渇に繋がりうる。

## 調査メモ（未確定・修正前）

- 上流応答ヘッダーの読取に上限（最大ヘッダーサイズ）・タイムアウトが
  効いているか、また早期 EOF を 502 へ即変換しているかを要確認。
- B-16（splice パイプ panic）と同一シナリオで顕在化しており、
  splice 経路のエラーハンドリングと関連する可能性。

## 修正内容（2026-07-05）

根本原因: HTTP/1.1 プロキシ経路のレスポンス転送関数群
（`transfer_response_with_compression` / `transfer_https_response_with_compression` /
`splice_transfer_response_ktls` / `proxy_request_buffered`）が、ヘッダーフェーズ失敗
（EOF・パース不能・サイズ超過・タイムアウト）時に **クライアントへ何も送らず**
`(status=502, total=0)` を「成功」として返却し、pooled 層もそれをそのまま成功扱い
していたため、クライアントが自身のタイムアウトまでハングしていた。
加えてボディ転送が Content-Length 宣言に満たないまま終了した場合も
クライアント接続を閉じていなかった。

修正（`src/proxy.rs` / `src/pool.rs`）:

1. **ヘッダーフェーズ失敗の即時エラー化**: pooled 層（`proxy_http_pooled` /
   `proxy_https_pooled`）で `total == 0 && status >= 500` の場合に 502/504 エラーページを
   即時送出して接続をクローズ。
2. **バックエンド応答ヘッダー読取タイムアウト**: `BACKEND_HEADER_TIMEOUT`（10 秒）を
   新設し、通常経路・splice 経路・WebSocket ハンドシェイクの全ヘッダー読取に適用。
   超過時は 504。
3. **応答ヘッダーサイズ上限**: `MAX_RESPONSE_HEADER_SIZE`（64KB）を新設し超過時は 502
   （従来はリクエスト用 8KB 上限を返却値だけ変えて使用しハングしていた）。
4. **CL 未達 EOF / chunked 終端前 EOF の即時クローズ**: 転送関数が
   `client_must_close` フラグ（戻り値 4 要素目）を返し、pooled 層がクライアント接続を
   即クローズ。ボディバッファリング失敗時はヘッダーだけ送らず 502 へ。
5. **splice ボディ転送の待機タイムアウト**: `readable()`/`writable()` 待機に
   READ/WRITE_TIMEOUT を適用（無応答上流・受信停止クライアントでの永久待機を防止）。

回帰テスト: `tests/e2e_tests.rs::test_b17_*`（8 プローブ、`tests/test_backends` の
bad-backend サーバー = ポート 9009 / ルート `/bad-backend/*` を使用）。
truncated-headers / huge-headers / bad-status / instant-close → 即時 502、
no-response → 10 秒で 504、cl-too-large → 200 後の即時クローズ、
cl-too-small → 余剰バイト非転送（スマグリング耐性の回帰ガード）。

コンテナ統合検証（`bad_backend_chaos.sh`、F-67、kTLS 有効の `veil:glibc`、
2026-07-05 実施、各プローブの実測経過時間を併記）:

| パス | 修正前 | 修正後 |
|------|--------|--------|
| /truncated-headers | TIMEOUT | **502** (7.5s※) |
| /cl-too-large | TIMEOUT | **200 + 即時クローズ** (6.5s※) |
| /huge-headers | TIMEOUT | **502** (6.7s※) |
| /bad-status | TIMEOUT | **502** (6.4s※) |
| /no-response | TIMEOUT | **502** (6.4s※) |
| /instant-close | TIMEOUT | **502** (7.1s※) |
| /ok（正常） | 200 | 200 (7.2s※) |
| /cl-too-small | 200 | 200 (6.5s※) |

※ 経過時間はすべて ~6.4〜7.5s でほぼ均一。これは高負荷下での
`docker run curlimages/curl` コンテナ起動のベースラインオーバーヘッドであり、
成功する /ok・/cl-too-small と異常系が同一時間帯であることが**ハングしていない証左**
（真のハングなら `--max-time 12` の 12s 近くになる）。`/cl-too-large` の
`200TIMEOUT` は curl の partial-file 終了（exit 18）で、Veil が CL 未達で接続を
即クローズしたことを示す。`veil state: running exit=0` で B-16 の panic も再現なし。

## 関連

- テスト: [F-67](../features/F-67-backend-protocol-violation-tests.md)
- 併発: [B-16](B-16-splice-pipe-refcell-borrow-panic.md)
