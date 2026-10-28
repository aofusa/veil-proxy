# B-256: HTTP/3 のサーバが quiche の `path_event_next()` を一度も呼ばず、相手の送信元の変更ごとに PathEvent が溜まる（GHSA-4q5x-gp38-rfp4、CVE-2026-12707）

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27〜28 のセキュリティレビュー 3 回目の SC-06（`docs/artifacts/security/analysis/11-supply-chain-vendored-advisories.md`、git 管理外）。

## 事象

- 上流の勧告（quiche 0.29.3 で修正）: ハンドシェイク後に相手が送信元アドレス（ポート）を次々に変えると、`PathEvent::ReusedSourceConnectionId` が
  `path_event_next()` で取り出されるまで無制限に溜まる。**サーバがアクティブマイグレーションを無効にしていても起きる**。
- veil（`src/http3_server/`）は `set_disable_active_migration(true)` だが、**`path_event_next()` を一度も呼ばない**（`grep` で 0 件）。
  修正版の quiche でも「アプリが消費しないキュー」として接続の寿命の間イベントが残り、0.24.9 では相手の送信元ポートの変更ごとに増える。
  UDP の送信元ポートは相手が自由に変えられる。

## 影響

- **P1**: 未認証のリモート（ハンドシェイク完了後）が 1 接続で HTTP/3 ワーカーのメモリを増やし続けられる。1 要素は小さいが、上限が無い。

## 改修案

1. メインループ（`src/http3_server/event_loop.rs` の受信処理の後）で接続ごとに `while conn.path_event_next().is_some() {}` で捨てる（ログは `debug!`）。
   これだけで「取り出すまで」という上限が効く。
2. 上流コミット `439795b00f`（fix: bound PathEvent queue by deferring events until path insertion）を `third_party/quiche` へバックポートする（B-255 と同じ PR でよい）。
3. `[http3]` に 1 接続のパス変更の回数上限（例: `max_path_changes`、既定 16）を足し、超えたら `CONNECTION_CLOSE` にする（quiche の
   `set_active_connection_id_limit` は相手の CID の数の上限で、送信元の変更回数は制限しない）。文書（英日）と `examples/config.toml` を更新。

## 受け入れ条件

- 単体: 送信元ポートを変えたパケットを多数処理しても `path_event_next()` の未消費の件数が 0 のまま、または上限で接続が閉じること。
- `tools/container_security` の `http3` フェーズが通る。

## 修正（v0.10.0）

- vendoring の quiche に upstream の `439795b00f`（PathEvent をパスの挿入まで遅らせてキューを抑える）を当てた（B-255 と同じ差分 7、0.24.9-veil.4）。
- veil 側: `event_loop.rs` の受信処理で `conn.recv` の後に `path_event_next()` を空になるまで取り出す（`debug!` のみ。veil はパスのイベントを使わない）。空のキューの確認だけなのでホットパスの費用は無視できる。
- 試験: 上流のキューの上限の試験（vendoring 版の `tests.rs`）が通る。E2E（HTTP/3 を含む）は io_uring・epoll とも 575/575。
- 残り: 改修案 3（`max_path_changes`）は入れていない（上流の修正でキューは抑えられ、veil も取り出すので、パス変更の回数そのものの上限は F-283 で扱う）。

## 関連

- [B-255](B-255-vendored-quiche-h3-frame-length-preallocation-ghsa-4fgf.md)、[B-222](B-222-http3-no-connection-limit-no-retry.md)、[B-94](B-94-http3-retransmitted-initial-new-connection.md)（決定的 SCID）
