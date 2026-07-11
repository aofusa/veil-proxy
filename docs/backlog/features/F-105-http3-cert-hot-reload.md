# F-105 HTTP/3 (QUIC/quiche) 証明書の無停止ホットリロード

- 優先度: P1
- 対応状況: 完了
- 起点レポート: [docs/artifacts/http3_cert_reload_implementation_plan.md](../../artifacts/http3_cert_reload_implementation_plan.md)
- 関連: F-03（0 ダウンタイム TLS 証明書更新 / HTTP/1.1・HTTP/2）、F-49（リロード E2E）

## 機能説明・現状

HTTP/1.1・HTTP/2 は `src/tls_reload.rs` の `GLOBAL_TLS_CONFIG`（`ArcSwap<Option<Arc<ServerConfig>>>`）
により、SIGHUP / mtime 変化で証明書を無停止更新できる（F-03）。アクセプタが毎ハンドシェイクで
`current_global_tls_config()` のスナップショットを取るため、既存接続は影響を受けず新規接続のみ新証明書を使う。

一方 **HTTP/3（`quiche` ベース）** は、ワーカースレッド起動時に `Http3ServerConfig` から
`quiche::Config` を一度だけ構築（`Rc<RefCell<quiche::Config>>`）し、以後更新する経路が無い。
SIGHUP を受けても HTTP/3 ワーカー内部の `quiche::Config` は差し替わらず、証明書更新には
プロセス再起動が必要だった。

## 改修内容

1. **HTTP/3 証明書のグローバル配信（`src/tls_reload.rs`）**
   - `Http3CertMaterial`（cert PEM / key PEM を `Mutex<Vec<u8>>` で保持、世代番号、未適用ワーカー数）を新設。
   - `GLOBAL_HTTP3_CERTS: ArcSwap<Option<Arc<Http3CertMaterial>>>` に **cert/key ペアをアトミックに** 配信。
   - `HTTP3_CERT_GENERATION: AtomicU64`（世代ゲート）と `HTTP3_WORKER_COUNT: AtomicUsize`（起動時に各ワーカーが登録）。
   - TLS リロードスレッド（`reload_now`）が HTTP/2 用 `ServerConfig` 更新に加え、HTTP/3 ワーカーが 1 台以上
     登録されている場合に **cert/key の生 PEM を読み込み `publish_http3_certs()` で配信** する。

2. **ワーカースレッドの軽量な変更検知（`src/http3_server.rs`）**
   - イベントループ先頭で `HTTP3_CERT_GENERATION` を `Acquire` ロードするだけの安価なゲート
     （u64 の atomic load は x86 で Relaxed と同等コスト。ArcSwap::load を毎周回呼ぶオーバーヘッドを回避）。
     ローカル世代と一致すれば何もしない。差分がある場合のみ `GLOBAL_HTTP3_CERTS` を参照する。

3. **quiche::Config の動的更新 + memfd 活用（Landlock 互換）**
   - 検知時のみ、既存の `create_memfd_for_pem`（`/proc/self/fd/<fd>` 経由・FS 非経由）で cert/key を
     memfd に載せ、`quic_config.borrow_mut().load_cert_chain_from_pem_file(...)` /
     `load_priv_key_from_pem_file(...)` で差し替える。memfd はロード直後にクローズ。
   - これは数ヶ月に 1 回の **コールドパス**。パース + memfd 書き込みで数 ms ループをブロックするが、
     ホットパス絶対規則の明示的な例外として理由コメントを添える。
   - 既存 QUIC 接続は `quiche::accept` 時に SSL_CTX から複製済みのため影響を受けず、
     以後の新規ハンドシェイクのみ新証明書を使う（HTTP/2 と同じ無停止セマンティクス）。

4. **秘密鍵の長期メモリ滞留リスク対策（最重要）**
   - `Http3CertMaterial` は配信時に「未適用ワーカー数」= 登録ワーカー数を保持。
   - 各ワーカーは quiche への適用完了後に `worker_applied()` を呼び、`fetch_sub(1, AcqRel)`。
   - **最後のワーカー（0 到達）が cert/key PEM を `secure_zero`（volatile 書き込み + fence）でゼロ化** し、
     グローバルからも平文を消す。全ワーカーの適用完了を AcqRel の happens-before で保証してからゼロ化するため、
     並行読み取りとの競合は発生しない。

## 受け入れ条件

- SIGHUP（`tls.auto_reload = true`）で HTTP/3 ワーカーの `quiche::Config` が差し替わり、
  以後の新規 QUIC ハンドシェイクが新証明書を提示する。既存接続は継続。
- 秘密鍵の平文が全ワーカー適用後にゼロ化される。
- Landlock 有効時も memfd 経由で FS を介さずロードできる。
- 各 feature 組み合わせ（full / default / no-default / http3 単体）でビルド warning・clippy・fmt クリーン。
- 単体テスト（tls_reload の HTTP/3 配信・ゼロ化・世代進行）を追加。

## 依存・リスク

- HTTP/3 の SIGHUP リロードは HTTP/2 と同じく `tls.auto_reload = true` が前提（リロードスレッドが起動する条件）。
- リロードスレッドは cert/key ファイルを再読込するため、Landlock 有効時は cert/key ディレクトリを
  `landlock_read_paths` に含める必要がある（HTTP/2 auto_reload と同条件。config.toml / README に明記）。
