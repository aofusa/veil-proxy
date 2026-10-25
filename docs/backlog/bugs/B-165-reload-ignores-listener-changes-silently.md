# B-165: リロードで待ち受けの設定を変えても黙って無視される

**状態: 完了（2026-10-25）**

## 事象

SIGHUP のリロード（`src/config/loaded.rs` `reload_config` → `load_config_without_tls`）は、`[server] listen`・`listen_cleartext`・
`threads`・`[http3] enabled/listen`・`[[l4]]`・`http2_enabled` の ALPN（[B-164](B-164-reload-http2-disabled-alpn-mismatch.md)）を
読み直さない（`LoadedConfigWithoutTls` に項目が無く、`l4_listeners` は今の値を複製する）。ログにも何も出ないので、
運用者は変更が効いたと思う。`docs/guide/library.md` 209 行には「再起動が要る」とあるが、本体のガイドと実行時の表示に無い。

## 改修案

- リロード時に、待ち受けに関わる項目（F-260 以降は正規化したリスナーの列）を今の値と比べ、違えば
  `warn!("listener settings changed (...); restart veil to apply them")` を 1 回出す（項目名を列挙）。リロード自体は続ける
  （ルート等の変更は反映する。今の挙動を保つ）。
- `veil -t` は対象外（比べる相手が無い）。admin API のリロード応答にも同じ警告を載せる。
- 文書（`docs/guide/running.md`・`configuration.md`、英日）に、リロードで変えられない項目の一覧を書く。
- 試験: 統合試験でリロード前後に listen を変え、警告が出て古いポートのままであること。

## 関連

- [F-260](../features/F-260-multiple-listeners.md)

## 実施内容

`src/config/loaded.rs` に `ListenerSettings`（listen・listen_cleartext・threads・http2_enabled・[http3] enabled/listen・[[l4]] の name+listen+protocol）を追加。起動時の値を `OnceLock` に持ち、リロードのたびに比べて違えば項目名つきで `warn!` を 1 回出す。単体試験あり。`docs/guide/running.md`（英日）に一覧を追記。admin のリロードはフラグを立てるだけで応答を持たないため、警告はログのみ。
