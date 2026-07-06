# B-23: HTTP リクエストスマグリング（CL.TE）— Content-Length: 0 + Transfer-Encoding: chunked の取りこぼし

## 事象

HTTP/1.1 リクエスト経路で、`Content-Length` と `Transfer-Encoding: chunked` を同時に持つ
リクエストのうち **`Content-Length: 0`** の場合が拒否されず、Veil が chunked として本文を
読みつつ、バックエンドへは **クライアント由来の `Content-Length: 0` を転送 + `Transfer-Encoding:
chunked` を再付与**していた。フロントエンド（Veil）とバックエンドが本文境界を別々に解釈する
**HTTP リクエストスマグリング（CL.TE デシンク）** の要因となる。

## 再現手順

```
POST / HTTP/1.1
Host: localhost
Content-Length: 0
Transfer-Encoding: chunked

5
hello
0

```

修正前: 400 にならずバックエンドへ転送（`Content-Length: 0` + `Transfer-Encoding: chunked`
の曖昧メッセージ）。CL を優先するバックエンドは本文なしと解釈し、`5\r\nhello...` を
次のリクエストとして扱う → スマグリング。

## 調査メモ

- 拒否判定 `src/proxy.rs` の `handle_requests` は `if content_length > 0 && is_chunked` で
  あり、**`content_length == 0`** のとき発火しなかった。
- 転送経路 `handle_proxy` は `Transfer-Encoding` を hop-by-hop として除去し chunked 時に
  再付与するが、**`Content-Length` は hop-by-hop ではない**ためクライアント値をそのまま
  バックエンドへ渡していた。
- RFC 7230 §3.3.3: CL と TE が両方ある場合は CL を無視すべきで、プロキシは曖昧さを
  排除する責務がある。既存の純関数 `validate_http_headers`（CL+TE 検査）は
  **テストのみで使われ実経路から呼ばれていなかった**（dead）。

## 改修

- `src/http_utils.rs` に **`classify_request_framing`**（`RequestFraming` を返す純関数、
  1 パス・ゼロアロケーション）を新設し、次を一律 400 で拒否:
  - 複数 Content-Length（§3.3.2）
  - **Content-Length と Transfer-Encoding の同時指定（CL 値に依らない＝CL:0 含む）**
  - Transfer-Encoding があるが最終エンコーディングが chunked でない（本文長不確定＝TE.CL 対策）
- `handle_requests` を本関数へ置換（従来の不完全な `content_length > 0 && is_chunked` を撤去）。
- 多層防御: `handle_proxy` の転送ループで **chunked 時にクライアント Content-Length を除去**。

## テスト

- 単体: `http_utils::chunked_span_tests::framing_*`（6 件。CL:0+TE・複数 CL・終端非 chunked・
  複数 TE ヘッダー連結・正常系）。
- E2E: `tests/e2e_tests.rs::test_request_smuggling_cl_te_rejected`（4 ベクタが 400）、
  `test_request_smuggling_legitimate_framing_allowed`（単独 chunked は誤検知しない）。

## 影響

- 優先度 P1（プロキシ特有の高リスク＝リクエストスマグリング）。修正でフロント/バックエンドの
  フレーミング解釈差を排除。

## 関連

- 検出元: [F-76](../features/F-76-http-smuggling-active-tests.md) / [F-66](../features/F-66-dast-owasp-zap.md)。
