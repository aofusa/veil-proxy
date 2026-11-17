# B-412: パスの正規化で、16 進 2 桁の続かない `%` と戻した文字が新しいエスケープ（`%2%45` → `%2E`）を作っていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Request-path normalization is not idempotent」から確認。

## 事象

- `path_norm::normalize` は `%XX` を 1 回の走査で戻す。16 進 2 桁の続かない `%` はそのまま写し、その後ろの
  予約されていない文字のエスケープは戻すので、`%2%45` は `%2E`、`%2%46` は `%2F`、`%5%43` は `%5C` になる。
- ドットのセグメントの除去（`remove_dot_segments`）は `.`・`..` だけを見て、B-385 の検査は生の入力の正しい
  `%2F`・`%5C` だけを見るので、どちらも素通りする。
- ルートの照合・`[route.security]` は `/public/%2E%2E/admin` を `/public/*` として扱い、そのまま上流へ送る。
  `%2E` を戻してからドットのセグメントを取り除く上流（nginx・Apache の既定）は `/admin` を返すので、
  パスごとのアクセス制御（`allowed_ips` 等）とルートの上流の接頭辞の外へ出られた。既定の `base` で起きる。
  HTTP/1.1・HTTP/2・HTTP/3 とメッシュ（xDS の `normalize_path`）の経路が同じ関数を通る。

## 対応

- 正規化が有効なとき、16 進 2 桁の続かない `%` は `%25` にする。出力の `%` は必ず自分の 16 進 2 桁を持つので、
  戻した文字と組み合わさって新しいエスケープにならない（正規化は冪等になる）。`%` の無いパスの経路は変えない。
- 利用者ガイド（`routing.md` の英日）と `examples/config.toml` の `path_normalization` の説明に書いた。

## テスト

- `path_norm::tests::stray_percent_does_not_form_new_escapes`（`%2%45%2%45`・`%%32%45`・`%5%43` と冪等性）、
  `path_norm::tests::dot_segments_and_unreserved_decoding`（`/bad%zz` → `/bad%25zz`）。
- ファジングの `fuzz_api::path_norm_smoke` に不変条件を足した: 正規化の結果の `%` はすべて 16 進 2 桁を持ち、
  予約されていない文字のエスケープを含まず、もう一度正規化しても変わらない。
