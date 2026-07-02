# F-44: TLS バックエンドのストリーミング化（F-32 残件）

- **優先度**: P1
- **対応状況**: 未着手
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
