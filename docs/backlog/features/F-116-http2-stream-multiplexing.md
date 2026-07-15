# F-116: HTTP/2 ストリーム多重化（コネクション内並行処理）

- 優先度: P1
- 状態: 完了（2026-07-15）
- 起点: [docs/artifacts/h2_performance_analysis.md](../../artifacts/h2_performance_analysis.md)（HTTP/2 パフォーマンス低下の原因調査）

## 機能説明（事象）

HTTP/2 のフレーム処理ループ（`src/proxy.rs` の `handle_http2_requests`）が、リクエスト成立
（`process_frame` が `Some(req)` を返す）のたびにバックエンド往復〜レスポンス送出完了までを
その場で `await` しており、同一コネクション内の他ストリームのフレームを読めない
（アプリケーション層 Head-of-Line Blocking）。

このため h2load `-c100 -m10`（100 コネクション × 10 多重化）でも実効並列数は 100 に制限され、
HTTP/2（2704 req/s・平均レイテンシ 186ms）が HTTP/1.1（3298 req/s・29ms）を下回っていた
（`docs/perf/README.md` の直交表 `h2_1_ktls_0_lb_kernel_ofc_1` 計測）。

## 改修内容

HTTP/3（`src/http3_stream.rs`、F-32）で実証済みの **単一スレッド・アクターモデル** を
HTTP/2 サーバ経路へ移植する。

- メインループが `Http2Connection`（ソケット・HPACK・フロー制御状態）を専有し、
  フレーム受信・全フレーム送出を一元化する。
- リクエスト成立時にリクエスト処理を **ストリームごとの独立タスク**（`TaskPool` で spawn）へ
  移譲し、レスポンスは有界チャネル（`H2RespMsg`）+ `Notify` でメインループへ返す。
- メインループの待機は「ソケット readable（`POLL_ADD`、キャンセル安全）or タスクからの
  notify」の select とし、in-flight RECV の drop（既読データ破棄）を発生させない。
- DATA 送出はフロー制御ウィンドウ内のみ書き込み、残りはストリーム状態へ保留して
  WINDOW_UPDATE 受信時に再駆動する（`send_data` の「ウィンドウ枯渇時にインラインで
  フレームを読む」実装はサーバ経路から廃止）。

詳細設計は [docs/artifacts/h2_multiplexing_design.md](../../artifacts/h2_multiplexing_design.md)。

## 期待効果

- 1 コネクション内の全ストリームが並行にバックエンド I/O・ファイル I/O を実行し、
  I/O 待ちが隠蔽される（実効並列数 100 → 1000）。
- 複数ストリームのレスポンスが `write_buf` へ自然に合流し、ソケット書き込み回数
  （io_uring SEND）がコネクション単位で償却される。
- HTTP/1.1 経路・HTTP/3 経路には変更を加えない（HTTP/1.1 非劣化をベンチで確認）。

## 検証

- 単体テスト（connection.rs の新 API・フロー制御保留）
- E2E: 既存 http2 テスト群 + 多重化並行性テスト（遅い応答が他ストリームを塞がないこと）
- h2spec（`tools/container_security/run_h2spec.sh`）
- perf: `CONFIG_GLOB='h2_1_ktls_0_lb_kernel_ofc_1'` で HTTP/2 改善・HTTP/1.1 非劣化を実測

## 検証結果（2026-07-15 完了時）

- 単体 751 件 / E2E フルスイート **530 passed / 0 failed**（新規
  `test_http2_multiplexing_slow_stream_does_not_block_fast` 含む。同一コネクションで
  遅延 1.5s ストリームと並行の高速ストリームが遅延を待たず完了することを実測）
- h2spec（`tools/container_security/run_h2spec.sh`）: TLS / H2C 両ゲート **52/52 passed, 0 failed**
  （2026-07-15、F-117 適用後のビルドで実施）
- 全 feature 組み合わせ（full / default / no-default / http2 / http2,grpc-full）ビルド警告 0、
  `clippy -D warnings` / `cargo fmt` クリーン、`allow(dead_code)` 不使用
- perf A/B（同日・同一環境、main / 本ブランチをイメージ再ビルドの上で連続計測）:
  **HTTP/2 +16.1%（3140.6 → 3646.2 req/s、glibc、h2load `-t4`）で HTTP/1.1（3214）超え・
  nginx 比 1.47 倍**。musl +9.6%。HTTP/1.1 は ±0（非劣化）。h2load 既定 1 スレッドは
  クライアント律速（+6.5% に見える）ため `-t4` 併用（`docs/perf/README.md` 参照）
- レビュー修正: アップロード recv ウィンドウ補充の消費連動化（受信即補充による
  バックプレッシャ喪失 = メモリ DoS 退行の防止）、トレイラー終端の req チャネル EOF、
  Head 未送出タスク終了時の RST_STREAM(INTERNAL_ERROR)、ストリーム打ち切り時の
  conn ウィンドウクレジットリーク防止
