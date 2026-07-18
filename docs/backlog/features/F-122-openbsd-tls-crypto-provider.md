# F-122: OpenBSD で TLS ハンドシェイクが aws-lc-rs で完了しない（ring プロバイダ等で解消）

- 優先度: P2
- ステータス: 未着手
- 起点: F-120 Phase 5（OpenBSD 対応）の VM 検証

## 事象

OpenBSD 7.9 (amd64) で veil をビルド（warning 0）・起動でき、kqueue ランタイム・
pledge・unveil は正しく動作するが、**TLS ハンドシェイクが完了しない**。

VM での ktrace 実測（`/work/vtest/plain.toml`、静的配信 HTTPS）:

1. `accept4` で接続を受理（fd 7）— リアクタ正常。
2. kqueue が fd 7 の可読イベント（`data=1519`）を配送 — kqueue 正常。
3. `read(7)` が ClientHello 1519 バイトを取得 — 読み取り経路正常。
4. **その後 `write(7)`/`send(7)` が一度も発行されない**（ServerHello が生成されない）。
5. ワーカースレッドは ~15% CPU で処理を続けるが応答せず、~17 秒で接続がクローズ。
   curl は `code=000`。

同一の rustls + aws-lc-rs（ユーザ空間 TLS、kTLS 非使用）は **Linux・FreeBSD では
正常に HTTPS を提供**するため、veil 側のリアクタ/TLS 駆動コード（`simple_tls`）の
問題ではなく、**aws-lc-rs（AWS-LC の C/アセンブリ暗号ライブラリ）が OpenBSD で
ハンドシェイク暗号処理を完了できない**（ハング、または実用不能なほど遅い）ことが原因。
OpenBSD は Rust Tier 3 かつ AWS-LC の OpenBSD サポートが不完全と考えられる。

## 改修案

1. **rustls の暗号プロバイダを OpenBSD では `ring` に切り替える**（第一候補）。
   `ring` は OpenBSD で広く実績がある。`rustls` の provider を cfg で
   `aws_lc_rs`（Linux/FreeBSD）/ `ring`（OpenBSD）に分岐する。
   - 注意: `aws-lc-sys` は `http3`（quiche）と AWS-LC ビルドを共有するため、
     OpenBSD で `ring` provider にする場合、http3 は OpenBSD で無効化するか
     別途 quiche の TLS backend を検討する（OpenBSD http3 は現状も未検証）。
   - `rcgen`（テスト用証明書生成）も aws_lc_rs 依存のため OpenBSD テストでは
     ring feature へ切り替える。
2. あるいは `aws-lc-sys` の OpenBSD ビルド設定（アセンブリ有効化・CPU feature 検出・
   スレッド初期化）を精査して aws-lc 自体を OpenBSD で動作させる。

## 現状の切り分け（F-120 Phase 5 で確認済み）

- OpenBSD ビルド: `--no-default-features --features "http2,mimalloc"` および
  http3/grpc-full/compression/cache/metrics/websocket/rate-limit/buffering/admin/
  access-log/l4-proxy を含む構成で **warning 0**。
- kqueue reactor: accept/read/イベント配送すべて動作（ktrace で確認）。
- pledge/unveil: 起動時に適用されログ出力を確認（`unveil: locked` / `pledge: promises restricted`）。
- ブロッカーは TLS 暗号（aws-lc-rs）のみ。
