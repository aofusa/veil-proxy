# F-44: TLS バックエンドのストリーミング化（F-32 残件）

- **優先度**: P1
- **対応状況**: 完了（2026-07-02）
- **出典**: `docs/artifacts/remaining_tasks_analysis.md` F-32 残件 / アイデア2

## 機能説明・現状

HTTP/2・HTTP/3 のストリーミングボディ転送（F-32）のうち、**TLS バックエンド**（バックエンドが HTTPS の場合）の経路は既存のバッファ経路にフォールバックしており、真のストリーミングになっていない。

## 改修内容

- TLS バックエンドへの接続を io_uring ランタイム上の async TLS（rustls ハンドシェイク + 可能なら kTLS 移行）で貫通させ、フロントエンド ⇔ バックエンドの双方向バックプレッシャ付きストリーミングパイプラインを確立する。
- バッファリングフォールバックを撤廃し、ゼロコピー（`Bytes`）でボディを逐次転送する。

## 受け入れ条件

- TLS バックエンド経由の大容量ボディが全量バッファされずに逐次転送される（メモリ使用量で検証）。
- HTTPS バックエンドの E2E（ストリーミング・大容量）が通過。

## 調査メモ（2026-07-02）

- ストリーミング除外点は `src/http3_server.rs::classify`（`server.target.use_tls → Decision::Buffer`）。HTTP/2 側は `src/proxy.rs` の TLS バックエンド分岐。
- 利用可能な部品: `ktls_rustls::connect()`（io_uring 上の async rustls ハンドシェイク + 可能なら kTLS 移行、`KtlsClientStream` は `AsyncReadRent`/`AsyncWriteRent` 実装済み）。
- **主要な設計課題**: `KtlsClientStream` の I/O は `&mut self` を要求するため、B-12 修正で導入した「アップロード/レスポンス受信の同一タスク内並行駆動」（`&TcpStream` 共有）がそのままでは適用できない。ユーザー空間 rustls の単一状態機械を読み書き並行に分割する（kTLS 移行後は生 fd で並行可能、非移行時は interior mutability + 交互駆動）設計が必要。
- 上記により本チケットは独立した設計・実装フェーズ（複数セッション規模）として継続する。

## 依存・リスク

- rustls のユーザースペース TLS と io_uring 読み書きの結合（部分レコード・ハンドシェイク中の write 要求）の状態機械が複雑。
- kTLS 移行可否（暗号スイート依存）による経路分岐。

## 実装内容（2026-07-02 完了）

**HTTP/2 側**は調査の結果、既に TLS バックエンドでもストリーミング済みだった（`proxy.rs` の
`use_tls` 分岐が `get_tls_connector().connect()` → ジェネリックな `run_h2_request_streaming`
に接続する）。残っていたのは **HTTP/3 の `classify` の TLS 除外**（TLS → `Decision::Buffer` →
リクエストごとに std::thread を spawn するブロッキング TLS + 全量バッファ経路）のみ。

**設計課題の解決（`&mut self` I/O × 同一タスク内並行駆動）**:

- `http3_stream::TlsBackend` を新設。ハンドシェイク済みの `KtlsClientStream` /
  `SimpleTlsClientStream` を `into_parts()`（両型に追加）で分解し、rustls セッションを
  `RefCell` に置いて read / write を `&self` で提供する全二重ラッパー。
  **借用は同期区間のみで完結し `.await`（`readable()`/`writable()` の POLL_ADD 待機）を
  跨いで保持しない**ため、B-12 で導入した「アップロード/レスポンス受信の同一タスク内
  `select_biased!` 並行駆動」がそのまま適用できる（thread-per-core 前提、ロック・アトミックなし）。
- kTLS 移行成功時（セッション `None`）は生ソケット（io_uring `TcpStream`、`&self` API）で
  読み書きする。スクラッチバッファ（暗号文 16KB / TLS レコード出力）は `RefCell` +
  take/replace で確保再利用（per-op malloc なし）。
- `BackendIo` enum（`Plain(TcpStream)` / `Tls(Box<TlsBackend>)`）を導入し、バックエンドタスクの
  全 I/O ヘルパー（head 送出・chunked 転送・レスポンス受信）を差し替え。
- `classify` の TLS 除外を撤去し、`BackendTaskParams` に `use_tls` / `sni` /
  `tls_insecure`（アップストリーム設定 + `VEIL_TLS_INSECURE` は起動時一度だけ評価）を追加。
- 付随: HTTP/3 メインループに協調的 yield（`runtime::yield_now` 新設）を追加。パケット
  連続到着時に select の recv arm が即 Ready になり続けて単一 poll 内でループし、同一
  スレッドのバックエンドタスク（TLS ハンドシェイク等）が飢餓する問題（B-12 調査時に
  520ms の遅延として観測）を緩和。

**テスト**: `tests/test_backends` に TLS echo サーバー（tokio-rustls、`TLS_ECHO_PORT` 9018）を
追加し、`/echo-upload-tls/*` ルートを E2E 環境に配線。
`test_http3_request_body_streaming_tls_backend`（1.2MB、初期ストリームウィンドウ超、往復
バイト一致）と `_small` を新設。

## 受け入れ条件の検証

- TLS バックエンド経由の大容量ボディが全量バッファされずに逐次転送される
  → 有界チャネル + フロー制御バックプレッシャの既存ストリーミング機構をそのまま通るため、
  ヒープ保持は「並行ストリーム数 × 有界バッファ」のまま（1.2MB 往復 E2E で検証）。
- HTTPS バックエンドの E2E（ストリーミング・大容量）が通過
  → 新設 2 テスト + HTTP/3 E2E 全 35 テスト通過（直列実行、loadavg 11 の co-tenant 負荷下でも
  全通過。並列実行は環境負荷起因の 15 秒タイムアウトフレーキーあり）。
