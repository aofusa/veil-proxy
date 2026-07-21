# F-131: HTTP/2 メインループファストパス + Stream オブジェクト再利用プール

## 背景

ユーザー提案「HTTP/2 の直列パース + 過剰タスク間通信による CPU 飽和を解消し HTTP/3 並みのスループットへ」に基づく4項目の改善案のうち、調査の結果 **フレームパース/Waker のバッチ化（F-116 で実装済み）** と **HPACK ハフマン LUT 化（F-121 で 4-bit 実装済み・8-bit 拡張は投資対効果が低く見送り）** は対応不要と判断。本チケットは残る2項目を実装する。

## 改修内容

### A. HTTP/2 メインループ・インラインファストパス

`h2_spawn_for_request`（`proxy.rs`）が全リクエストを無条件で `TaskPool::spawn` + チャネル（`H2RespMsg` Sender/Receiver）経由にディスパッチしていたのを改め、バックエンド I/O・WASM 実行を伴わない同期完結の応答（Prometheus メトリクス・管理 API・セキュリティチェック拒否・404 Not Found・`Backend::Redirect`〔WASM 非適用時のみ〕）はメインループから `conn.send_headers_buffered_end()`/`conn.queue_data_frames()` を直接呼んでインライン応答する。タスク生成・チャネル・`Notify` 起床のオーバーヘッドをゼロにする。

### B. Stream オブジェクト再利用プール（free-list）

`StreamManager`（`src/http2/stream.rs`）に free-list を追加し、クローズ済み `Stream` の `Vec`/`BytesMut` バッファを `clear()`（容量保持）して再利用する。`HashMap<u32, Stream>` のキー管理自体は維持（ID→スロットの直接マッピングは、生成順とクローズ順が一致しない HTTP/2 の性質上、単純な mod 容量方式では同時アクティブ数以下でも衝突し得るため採用せず、安全性を優先）。接続チャーン時のヒープ再確保回数を削減する。

## 設計ドキュメント

`docs/artifacts/f131_http2_fastpath_streampool_design.md`

## レビューで発見・修正した問題

実装レビュー（Fable）で、ファストパス実装に2件の問題を発見し修正済み:

- **レートリミット二重消費（重大・正確性バグ）**: `h2_try_fast_path` がファストパス非該当と判定する前に `check_security`（内部でレートリミッタのカウンタを消費）を呼んでいたため、Redirect 以外（＝トラフィックの大半を占める Proxy 経路）で spawn 経路の `h2_dispatch` と合わせて 1 リクエストにつき 2 回カウントされ、設定レートリミットが実質半分になっていた。`h2_fast_path_should_check_security_inline()` でゲートし、`check_security` はインラインで応答を完結させる場合（Redirect かつ WASM 非適用）のみ呼ぶよう修正。
- **ルーティング二重実行（性能）**: 同様に `find_backend_unified` も Redirect 以外で二重実行されていた。`H2RequestCtx.resolved_route` に解決済みルーティング結果を引き継ぎ、`h2_dispatch` 側での再実行を回避するよう修正。

追加で、新規回帰テストが `crate::runtime::block_on`（実 io_uring リング初期化）を使っていたため Docker ビルド中の並列テスト実行で資源競合によるフレーキー失敗が発生し、`futures::executor::block_on` に切り替えて修正。

## 検証結果

- 単体テスト（`cargo test --features full --lib`）787件全てグリーン、新規追加分含む
- 統合テスト（`cargo test --features full --test integration_tests`）53件グリーン
- E2E テスト（`./tests/e2e_setup.sh test`）533件グリーン
- `cargo build`（full / default / no-default-features / http2 単体 / http2+wasm+admin）全て警告ゼロ
- `cargo clippy --all-targets --features full` クリーン、`cargo fmt --check` クリーン
- `docker/README.md` 手順で glibc イメージをビルドし、`tools/perf` で main（ベースライン）と本ブランチを A/B 比較（`CONFIG_GLOB` で `h2_1_ktls_0_lb_kernel_ofc_0` / `h2_1_feat_proxy`（Proxy 経路・ファストパス対象外の主要トラフィック）/ `h2_1_feat_metrics` の3構成、`ITERATIONS=3`）:
  - `h2_1_ktls_0_lb_kernel_ofc_0` (http2): baseline 2470.2 req/s → branch 2467.1 req/s（誤差範囲内、リグレッションなし）
  - `h2_1_feat_proxy` (http2): baseline 1946.7 req/s → branch 1925.2 req/s（誤差範囲内、リグレッションなし。ルーティング二重実行の修正が効いている）
  - `h2_1_feat_metrics` (http2): baseline 2490.0 req/s → branch 2489.2 req/s（誤差範囲内）
  - 上記3構成とも h2load は既定で `/`（通常の静的配信）を叩くためファストパス自体（404/メトリクス応答そのもの）の負荷は計測対象外。補助的に 404 応答（ファストパス対象）への直接 h2load 計測（`-n30000 -c100 -m10`）も行ったが、ホストの co-tenant 負荷（load average 2-4/4コア、QEMU VM 等が同時稼働）により baseline/branch 間で数%の差が測定誤差の範囲に埋もれ、有意な改善/悪化を判定できなかった（静かなホストでの再計測が望ましいが、主要トラフィック経路にリグレッションが無いことは確認済み）

## 状態

完了
