# B-164: `http2_enabled` を true → false にリロードすると、h2 を選んだ TLS の接続が HTTP/1.1 の処理へ渡る

**状態: 未着手（2026-10-25 起票。F-260 の調査で見つけた。コードを読んで確認、実機の再現は着手時）**

## 事象（コードから）

- TLS の ALPN は起動時の `http2_enabled` で 1 回だけ作る（`src/config/tls_server.rs` `load_tls_config` の 198 行付近
  `configure_alpn_h2`）。証明書のリロード（`entry.rs` 530〜560 行の閉包）も **起動時の** `http2_enabled` を捕まえて作り直す。
  設定のリロード（`loaded.rs` `LoadedConfigWithoutTls`）は TLS の設定を作り直さない。
- 接続の振り分け（`src/proxy/detect.rs` 392・440 行付近）は **リロード後の** `CURRENT_CONFIG.http2_enabled` を読み、
  `if http2_enabled && tls_stream.is_http2()` のときだけ HTTP/2 の処理へ渡す。
- よって true で起動して false にリロードすると、ALPN は h2 を出し続け、h2 を選んだクライアントの接続（プリフェース）が
  `handle_requests`（HTTP/1.1）に渡る → 400 か切断。

## 影響

h2 を止めようとしてリロードした運用者の環境で、HTTP/2 のクライアント（ブラウザのほぼ全部）が失敗する。false → true の
リロードでは h2 は出ないまま（無害だが反映されない）。

## 改修案

- 振り分けはハンドシェイクで決まった ALPN だけで決める（`tls_stream.is_http2()`）。ALPN を出した以上、HTTP/2 で処理する。
- `http2_enabled` の変更はリロードで反映できないことを警告する（[B-165](B-165-reload-ignores-listener-changes-silently.md) と同じ扱い）。
  F-260 で ALPN をリスナーのプロファイルごとに持つようにしたら、リロードで作り直す設計に寄せてもよい。
- 試験: 統合試験で true で起動 → false にリロード → `curl --http2` が成功する（HTTP/2 のまま）こと、警告が出ること。

## 関連

- [F-260](../features/F-260-multiple-listeners.md)
