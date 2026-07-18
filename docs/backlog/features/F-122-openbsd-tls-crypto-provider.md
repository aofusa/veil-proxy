# F-122: OpenBSD で TLS ハンドシェイクが aws-lc-rs で完了しない（ring プロバイダで解消）

- 優先度: P2
- ステータス: **完了**（rustls の暗号プロバイダを OpenBSD 限定で ring に切替）
- 起点: F-120 Phase 5（OpenBSD 対応）の VM 検証

## 解決（実装済み）

**rustls の暗号プロバイダを OpenBSD では `ring`、非 OpenBSD（Linux/FreeBSD）では
`aws_lc_rs` に切り替えた**（`src/tls_provider.rs` で `pub use ... as provider`、
Cargo は target 別依存で完全分離）。LibreSSL（OpenBSD ネイティブ TLS）は rustls が
暗号バックエンドに使えず、TLS 終端全体を openssl クレートへ書き換える大改造となり
非現実的なため、rustls の 2 プロバイダのうち OpenBSD で実績のある `ring` を採用した。

- **Cargo.toml**: `rustls` を `default-features = false`（default に含まれる aws_lc_rs を
  暗黙有効化しない）とし、provider を target 別に付与（非 OpenBSD=aws_lc_rs + aws-lc-rs、
  OpenBSD=ring + ring クレート）。`resolver = "2"` を追加し、dev-dependencies
  （hyper-rustls/quinn/tokio-rustls/rcgen）の aws_lc_rs feature が通常ビルドへ統合されて
  OpenBSD 本体が aws-lc-sys を要求するのを防止（cargo tree -e normal で aws-lc 0 件を確認）。
- **src**: `default_provider()` / `ALL_CIPHER_SUITES` / http3 の `SystemRandom` の各参照を
  `crate::tls_provider::provider` / `crate::tls_provider::{SecureRandom, SystemRandom}` へ
  集約（entry.rs / config.rs / simple_tls.rs / tls_reload.rs / http3_server.rs）。
  kTLS 経路（ktls.rs / ktls_rustls.rs）は `veil_ktls`=Linux 限定のため aws_lc_rs のまま。
- **unveil 実装バグも同時修正**: `unveil_path` が `Path::exists()` で事前存在確認して
  いたが、unveil は最初の呼び出しでファイルシステムビューを制限するため、2 番目以降の
  パスの `exists()` が false になり 8 パス中 1 つしか unveil されず静的配信が 404 になって
  いた。`exists()` 事前確認を廃し unveil(2) を直接呼んで ENOENT のみ無視するよう修正。

### VM 検証（OpenBSD 7.9 amd64）

- ビルド warning 0（`http2,mimalloc`。normal 依存に aws-lc なし = ring のみ）。
- **静的配信 HTTPS 200**（`<h1>openbsd</h1>`、HTTP/2 over TLS、ALPN 動作）。
- **プロキシ経路 HTTPS 200**（`connect(2)` 動作、type=error 0）。
- pledge（`stdio rpath wpath cpath inet dns flock`）+ unveil（8 パス）**有効のまま**
  上記いずれも 200。

## 事象（元の調査記録）

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
