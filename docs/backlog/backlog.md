# Veil バックログ（親ドキュメント）

機能追加・バグの **一覧・優先度・対応状況** をここで管理する。個別の説明・改修案は **専用の md ファイル** に書き、本ファイルでは **目次とステータスのみ** を保つ。

## 運用ルール（必須）

| 種別 | 格納先 | 各ファイルに含める内容（目安） |
|------|--------|--------------------------------|
| **機能追加** | [features/](features/) | 機能説明、現状、**改修内容**、**改修案**、受け入れ条件、依存・リスク |
| **バグ報告** | [bugs/](bugs/) | 事象（再現手順）、影響、**調査メモ**、**改修案**、関連コミット・PR |

**追加時**: 上記ディレクトリに md を新規作成し、**本ファイルの該当表に行を追加**する（優先度・対応状況・リンク）。

**修正完了時**: 該当チケットの **対応状況** を更新する。必要なら個別 md に「完了日・リリースタグ」を追記する。

優先度はリリース計画に応じて見直す。個別ドキュメント間の相互リンクは [features/](features/) 内の相対パスでよい。

---

## 優先度の目安

| 記号 | 意味 |
|------|------|
| **P0** | セキュリティ・データ損失・全面停止に直結 |
| **P1** | 本番運用の主要シナリオを阻害 |
| **P2** | 改善・拡張で価値が大きいが回避策あり |
| **P3** | 長期・調査寄り、フェーズ 2 以降でもよい |

---

## 対応状況の目安

| 状態 | 意味 |
|------|------|
| **未着手** | 仕様・調査のみ |
| **進行中** | 実装・検証中 |
| **完了** | マージ済み・リリース方針に従いクローズ |
| **保留** | 優先度下落、外部依存、方針未決 |

---

## 機能追加チケット一覧

| ID | 優先度 | 対応状況 | ドキュメント | 概要 |
|----|--------|----------|--------------|------|
| F-02 | P1 | 完了 | [features/e2e-test-hardening.md](features/e2e-test-hardening.md) | E2E の網羅・実装乖離の解消（369 テスト中 368～369 通過、負荷フレーキー1件を除き全通過） |
| F-90 | P1 | 完了 | [features/e2e-coverage-expansion.md](features/e2e-coverage-expansion.md) | e2e_test_coverage.md ギャップ解消（430/430 通過、B-35〜B-37 修正、新規 E2E 11 件、`#[ignore]` 0 件） |
| F-03 | P1 | 完了 | [features/tls-cert-zero-downtime.md](features/tls-cert-zero-downtime.md) | 0 ダウンタイム TLS 証明書更新 |
| F-04 | P1 | 未着手 | [features/vds-xds-dynamic-config.md](features/vds-xds-dynamic-config.md) | 動的設定配信 API（VDS / xDS 相当） |
| F-06 | P1 | 完了 | [features/resilience-outlier-detection.md](features/resilience-outlier-detection.md) | サーキットブレーカー・リトライ・異常検知 |
| F-09 | P1 | 完了 | [features/prometheus-feature-flags.md](features/prometheus-feature-flags.md) | Prometheus 拡充と feature 無効化 |
| F-01 | P2 | 完了 | [features/grpc.md](features/grpc.md) | gRPC / gRPC-Web の完成度・テスト拡充 |
| F-05 | P2 | 未着手 | [features/acme.md](features/acme.md) | ACME 統合 |
| F-07 | P2 | 進行中 | [features/fuzzing-chaos-security.md](features/fuzzing-chaos-security.md) | ファジング・カオス・h2spec・セキュリティスキャン（`tools/container_security/` 基盤完了。F-52〜F-57 で拡充中） |
| F-52 | P1 | 完了 | [features/F-52-cargo-fuzz-libfuzzer.md](features/F-52-cargo-fuzz-libfuzzer.md) | cargo-fuzz（HPACK・frame・header・config + **スマグリング分類 `http_request_smuggling`**）。LibAFL 移行は F-82 の nightly CI 基盤とセットで再評価と判断。ASAN/corpus CI 化は F-82 へ分離 |
| F-53 | P1 | 完了 | [features/F-53-chaos-engineering-expansion.md](features/F-53-chaos-engineering-expansion.md) | カオス拡充（CB・slowloris・reset 完了、Pumba/tc は **F-69** で完了）。子 F-67/F-68 も完了。Toxiproxy 遅延下の生存・回復とタイムアウトを検証 |
| F-54 | P1 | 完了 | [features/F-54-security-scan-expansion.md](features/F-54-security-scan-expansion.md) | セキュリティスキャン（testssl・cargo-deny・SECURITY.md・ZAP(F-66)・SBOM(F-65)・gitleaks(F-75)）+ **seccomp 禁止 syscall 発火テスト**。Nuclei/Landlock 発火は F-83 へ分離 |
| F-55 | P2 | 完了 | [features/F-55-harness-hardening.md](features/F-55-harness-hardening.md) | ハーネス堅牢化（metrics リロード検知・レポート集約）。**GHA glibc/musl マトリクス統合を実装**（F-57 nightly、results artifact + Job Summary） |
| F-56 | P2 | 完了 | [features/F-56-property-load-tests.md](features/F-56-property-load-tests.md) | プロパティベース・負荷テスト。ルーティング不変条件（`tests/routing_proptest.rs`、実行中に **B-22** 検出・修正）+ **設定パーサ proptest**（`tests/config_proptest.rs`：ProxyTarget::parse ラウンドトリップ/決定性・test_config_file no-panic）+ **wrk/k6 負荷ハーネス**（`tests/load/`：baseline/chaos 比較・手順書） |
| F-57 | P2 | 完了 | [features/F-57-container-security-ci.md](features/F-57-container-security-ci.md) | container_security CI/CD 統合。**`.github/workflows/ci.yml`（fmt/clippy/feature マトリクス/ユニット・統合・プロパティ・E2E）+ `container-security-nightly.yml`（glibc/musl で run.sh・results/SBOM artifact・Job Summary）** を実装 |
| F-08 | P2 | 完了 | [features/proxy-wasm-benchmarks.md](features/proxy-wasm-benchmarks.md) | Proxy-Wasm ベンチマーク（`benches/wasm.rs`：`/wasm/*` 適用 vs 非適用のレイテンシ差でフィルタオーバーヘッドを計測。Keep-Alive で接続コスト償却） |
| F-10 | P1 | 完了 | [features/opentelemetry.md](features/opentelemetry.md) | OpenTelemetry 対応 |
| F-18 | P1 | 完了 | [features/l4-stream-proxy.md](features/l4-stream-proxy.md) | L4 (TCP/UDP) ストリームプロキシ |
| F-19 | P2 | 完了 | [features/advanced-load-balancing.md](features/advanced-load-balancing.md) | 高度なロードバランシング (Weighted, Consistent Hash等) |
| F-20 | P2 | 完了 | [features/proxy-cache-purge-advanced.md](features/proxy-cache-purge-advanced.md) | キャッシュのPurge機能・制御高度化 |
| F-21 | P2 | 完了 | [features/structured-access-log-admin.md](features/structured-access-log-admin.md) | 構造化アクセスログと管理Admin API |
| F-22 | P2 | 完了 | [features/enhanced-health-check.md](features/enhanced-health-check.md) | ヘルスチェックの強化 (Active probing, TCP) |
| F-23 | P1 | 完了 | [features/refactor-cargo-features.md](features/refactor-cargo-features.md) | Cargo.toml の features フラグ整理 |
| F-24 | P2 | 完了 | [features/F-24-zero-copy-pipeline-http2-http3.md](features/F-24-zero-copy-pipeline-http2-http3.md) | HTTP/2・HTTP/3 ゼロコピーパイプライン（kTLS splice 非同期化済み・HTTP/2 はボディ deep clone 排除） |
| F-25 | P1 | 完了 | [features/F-25-seccomp-bpf-prot-exec-validation.md](features/F-25-seccomp-bpf-prot-exec-validation.md) | seccomp BPF 引数レベル検証（mprotect/mmap で PROT_EXEC をブロック） |
| F-26 | P2 | 完了 | [features/F-26-http2-bytes-zero-copy.md](features/F-26-http2-bytes-zero-copy.md) | HTTP/2 ヘッダ/ボディのヒープ割り当て排除（bytes クレートゼロコピー化） |
| F-27 | P2 | 完了 | [features/F-27-wasm-instance-pooling-async-fuel.md](features/F-27-wasm-instance-pooling-async-fuel.md) | WASM 非同期実行（wasmtime async_support + Fuel Yield）+ pooling allocator |
| F-28 | P1 | 完了 | [features/F-28-custom-iouring-impl.md](features/F-28-custom-iouring-impl.md) | monoio 削除・カスタム io_uring 実装（thread-per-core、IORING_REGISTER_RESTRICTIONS） |
| F-29 | P1 | 完了 | [features/F-29-lockfree-cache-and-async-fs.md](features/F-29-lockfree-cache-and-async-fs.md) | ホットパスのロック排除・非同期FS・Range ゼロアロケーション化。canonicalize/metadata/ディスク読込を runtime::offload（専用スレッドプール+eventfd POLL_ADD）で完全非同期化（イベントループ非ブロック・新規オペコードなし） |
| F-32 | P1 | 完了 | [features/F-32-http2-http3-streaming-body.md](features/F-32-http2-http3-streaming-body.md) | HTTP/2・HTTP/3 レスポンス+リクエスト方向の真のストリーミング実装。HTTP/2 第1〜3: 非圧縮+content-length / chunked（`next_data_span` ゼロコピー）/ リクエスト方向 chunked 逐次転送。HTTP/3 第4: アクターモデル（メインループ=QUIC/H3 ⇔ バックエンドタスク=TCP I/O を Rc チャネル+Notify で接続、`select_biased!` 多重化）でレスポンス/リクエスト双方向を全バッファリング排除・双方向バックプレッシャ・bytes ゼロコピー。付随で rustls received_plaintext 16KB 上限超過バグ + EAGAIN ビジースピン + リクエスト framing 誤判定（GET の RST）を修正。TLS バックエンドのストリーミングのみ継続 |
| F-30 | P2 | 完了 | [features/F-30-l4-splice-zerocopy.md](features/F-30-l4-splice-zerocopy.md) | L4 ストリームプロキシの splice(2) ゼロコピー転送（E2E 追加→B-09 修正→pipe 経由 splice 実装。ユーザースペースバッファ撤廃） |
| F-31 | P2 | 完了 | [features/F-31-memory-cache-bytes-zerocopy.md](features/F-31-memory-cache-bytes-zerocopy.md) | メモリキャッシュの bytes::Bytes ゼロコピー配信 |
| F-33 | P3 | 完了 | [features/F-33-http3-gso-gro-offload.md](features/F-33-http3-gso-gro-offload.md) | HTTP/3 送信 GSO バッチング + 受信 GRO 配線（recv_gro_async）。受信ループの 64KB バッファを再利用し per-datagram の 3 確保 + 2 コピーを排除（ゼロコピー受信）。送信も単一パケット to_vec 排除 + スレッドローカル送信スクラッチ再利用 |
| F-34 | P3 | 完了 | [features/F-34-connection-state-slab-arena.md](features/F-34-connection-state-slab-arena.md) | executor のタスク管理をスラブ + index Waker へ全面書換（接続ごと Arc<Task> 確保・2 ロック・per-wake Arc クローンを排除）。HTTP/2 64KB バッファ + HTTP/3 送受信の per-op malloc も排除済み |
| F-35 | P3 | 一部完了 | [features/F-35-xdp-ebpf-ddos-defense.md](features/F-35-xdp-ebpf-ddos-defense.md) | ユーザースペース最前線（accept 段の IP ブロックリスト、TLS 前に切断）を実装。XDP/eBPF 本体は専用環境（CAP_BPF/対応NIC）が必要で継続 |
| F-36 | P3 | 完了 | [features/F-36-wasm-cwasm-aot-cache.md](features/F-36-wasm-cwasm-aot-cache.md) | WASM cwasm AOT 事前コンパイルキャッシュ |
| F-37 | P3 | 完了 | [features/F-37-runtime-optable-hotpath.md](features/F-37-runtime-optable-hotpath.md) | ランタイム最ホットパスの per-op コスト排除（OP_TABLE の SipHash→Fibonacci 軽量ハッシュ＋事前確保、user_data 採番を グローバルアトミック→スレッドローカル化で偽共有排除）。F-34 姉妹最適化 |
| F-38 | P1 | 完了 | [features/F-38-iouring-restrictions-security-integration.md](features/F-38-iouring-restrictions-security-integration.md) | io_uring オペコード制限の security.rs 統合と stale monoio スタブ解消（制限本体は F-28 でランタイム実装済み。dead stub 削除・報告修正・許可リストレビュー） |
| F-39 | P1 | 完了 | [features/F-39-http-proxy-iouring-splice.md](features/F-39-http-proxy-iouring-splice.md) | HTTP プロキシ層の libc::splice を io_uring 非同期 splice（IORING_OP_SPLICE）に統一（同期ラッパー削除・pipe 全量ドレインでデータ損失も修正） |
| F-40 | P2 | 完了 | [features/F-40-l4-pipe-threadlocal-pool.md](features/F-40-l4-pipe-threadlocal-pool.md) | L4 プロキシの splice パイプをスレッドローカルプールで再利用（接続ごと pipe2(2) 排除、FIONREAD 残データ検査で混線防止） |
| F-41 | P1 | 完了 | [features/F-41-proxy-per-conn-alloc-elimination.md](features/F-41-proxy-per-conn-alloc-elimination.md) | proxy.rs 接続ごとの client_ip / host:port アロケーション排除（IpStr/HostPortStr スタックフォーマッタで計 14 箇所置換、F-29 残件） |
| F-42 | P1 | 完了 | [features/F-42-buffering-async-fs-offload.md](features/F-42-buffering-async-fs-offload.md) | buffering/handler.rs の非同期 FS 化（write/read/remove 全操作を runtime::offload 適用、F-29 残件） |
| F-43 | P3 | 完了 | [features/F-43-wasm-hotpath-alloc-reduction.md](features/F-43-wasm-hotpath-alloc-reduction.md) | WASM パスのアロケーション削減（F-29 残件）。modules は Arc 共有・path/method/client_ip は Arc&lt;str&gt;・ヘッダは per-module deep copy を所有権ムーブスルー化で排除（ボディフィルタ経路の copy は残課題として明記） |
| F-44 | P1 | 完了 | [features/F-44-tls-backend-streaming.md](features/F-44-tls-backend-streaming.md) | TLS バックエンドのストリーミング化（F-32 残件）。HTTP/3 classify の TLS 除外を撤去し、全二重 TLS ラッパー（TlsBackend、RefCell 借用を await 跨ぎで保持しない設計）で貫通。旧「リクエストごと std::thread + ブロッキング TLS + 全量バッファ」経路を置換 |
| F-45 | P3 | 完了 | [features/F-45-http3-gro-batch-recv.md](features/F-45-http3-gro-batch-recv.md) | HTTP/3 GRO バッチの per-segment オーバーヘッド削減（RefCell 借用 1 回化・同一 DCID 判定スキップ。quiche recv は 1 データグラム API のため一括渡しは不可、F-33 残件） |
| F-46 | P3 | 完了 | [features/F-46-typed-task-pool-optable-slab.md](features/F-46-typed-task-pool-optable-slab.md) | executor の Box&lt;dyn Future&gt; 排除・OP_TABLE スラブ化（F-34 / F-37 残件）。OP_TABLE を index+世代パックのスラブへ置換（per-op ハッシュ排除、B-07 の detach 意味論は世代で担保）、型付き TaskPool で接続/リクエスト spawn の malloc をゼロ化。Sleep の in-flight drop リークも修正 |
| F-47 | P3 | 保留 | [features/F-47-xdp-ebpf-sandbox-env.md](features/F-47-xdp-ebpf-sandbox-env.md) | XDP/eBPF 隔離検証環境の構築とモジュール分離（F-35 残件、CAP_BPF / 対応 NIC の環境依存） |
| F-48 | P3 | 完了 | [features/F-48-proxy-wasm-benchmark-expansion.md](features/F-48-proxy-wasm-benchmark-expansion.md) | Proxy-Wasm ベンチマーク拡充（F-08 残件）。プール枯渇（並行度 2/8/32）ベンチ + fuel メトリクス（veil_wasm_fuel_consumed_total 新設）/RSS 自動レポートを実装。既存ベンチが 404 パスで全スキップされていたバグも修正。「HTTP コールあり」はホストの Pause/resume 未配線のため対象外と明記 |
| F-49 | P1 | 完了 | [features/F-49-reload-e2e-verification.md](features/F-49-reload-e2e-verification.md) | 設定ファイル・TLS 証明書リロードの正常性確認 E2E テスト（SIGHUP 実送出でルート反映・不正設定フェイルセーフ・証明書差し替え/ゼロダウンタイムを検証） |
| F-50 | P1 | 完了 | [features/F-50-tls-cipher-suites-config.md](features/F-50-tls-cipher-suites-config.md) | [tls] cipher_suites 設定（nginx 風の取捨選択・優先度指定。リロード経路にも伝搬、E2E でネゴシエーション検証） |
| F-51 | P1 | 完了 | [features/F-51-config-toml-sync.md](features/F-51-config-toml-sync.md) | config.toml を src/config.rs と完全同期（route.security/WASM capabilities 等 19 キー追記、stale な [grpc] セクションと dead な RetryPolicy を削除、同期保証テスト追加） |
| F-58 | P1 | 完了 | [features/F-58-perf-report-glibc-musl-nginx.md](features/F-58-perf-report-glibc-musl-nginx.md) | パフォーマンス測定レポート（glibc/musl/nginx 比較・B-13/B-14/B-15 修正後の再測定）。全 24 計測 Non-2xx=0、レポート `docs/artifacts/performance_report_veil_vs_nginx_v2.md` |
| F-59 | P2 | 完了 | [features/F-59-writev-scatter-gather-cache.md](features/F-59-writev-scatter-gather-cache.md) | ヘッダ + ボディの scatter-gather 1-syscall 送出。**実装済み**: `IORING_OP_SENDMSG` を許可リストへ追加（サーフェス拡大を許容する判断）、`SendMsgFuture`（msghdr/iovec の Box 固定 + B-07 detach 延命 + 状態プール）、short-write 継続、キャッシュヒット・プロキシ応答ヘッダ+初期ボディへ配線。kTLS/rustls はフォールバック |
| F-60 | P3 | 完了 | [features/F-60-http3-gro-batch-autosize.md](features/F-60-http3-gro-batch-autosize.md) | HTTP/3 GRO 一括 recv・GSO/GRO セグメントサイズ自動調整（F-33 残件）。**実装済み**: GSO セグメントを quiche PMTU 探索へ per-connection 追従（クランプ 1200〜65507）。GRO 側は F-45 時点で最適点を確認。実装中に **B-18 を検出・修正** |
| F-61 | P3 | 完了 | [features/F-61-wasm-body-filter-alloc-reduction.md](features/F-61-wasm-body-filter-alloc-reduction.md) | WASM ボディフィルタ経路のアロケーション削減（F-43 残件）。**実装済み**: `BodyBuffer`（Shared Bytes / Owned Vec の CoW）を新設し、エンジン API・ホスト関数を Bytes ベース化。読み取りのみのモジュールでボディ deep copy ゼロ |
| F-62 | P3 | 完了 | [features/F-62-proxy-wasm-http-call-benchmark.md](features/F-62-proxy-wasm-http-call-benchmark.md) | Proxy-Wasm「HTTP コールあり」フィルタのベンチマーク（F-48 残件）。**実装済み**: Pause → インライン上流コール（offload 退避）→ 同一インスタンスで proxy_on_http_call_response resume の配線、http_call_filter.wasm + E2E 2 件 + ベンチ `wasm_http_call`。実装中に **B-19 / B-20 を検出・修正** |
| F-63 | P1 | 完了 | [features/F-63-log-output-routing.md](features/F-63-log-output-routing.md) | ログ出力先の分離ルーティング（app=stdout / error=stderr / access=stdout をレベル別に振り分け、`type` 識別フィールド付与、JSON 順は timestamp→level→type、ログファイル親ディレクトリを landlock_write_paths へ自動追加） |
| F-64 | P2 | 完了 | [features/F-64-sast-semgrep.md](features/F-64-sast-semgrep.md) | SAST（semgrep）導入。`run_semgrep.sh` + **Veil カスタムルール `.semgrep/veil-rules.yml`（static-lifetime transmute=B-16 番人・bare allow(dead_code)）を整備・配線**。CI 差分ゲートのみ残件（F-54 子） |
| F-65 | P2 | 完了 | [features/F-65-sbom-generation.md](features/F-65-sbom-generation.md) | SBOM 自動生成（syft）。source CycloneDX 823 件 + image SPDX 7 件を生成。**CI（F-57 nightly）で artifact 添付を実装**。残件のgrype連携・GitHub Release添付は外部インフラ・CIタスクとしてF-81へ分離したため完了（F-54 子） |
| F-66 | P2 | 完了 | [features/F-66-dast-owasp-zap.md](features/F-66-dast-owasp-zap.md) | 高度な DAST（OWASP ZAP Baseline）。`run_zap.sh` 追加・配線。スマグリング能動テストは F-76 で実装（Active Scan トグルのみ残件、F-54 子） |
| F-67 | P1 | 完了 | [features/F-67-backend-protocol-violation-tests.md](features/F-67-backend-protocol-violation-tests.md) | バックエンドのプロトコル違反テスト。`bad_backend_chaos.sh`（B-16/B-17 検出・修正済）+ **H2C クライアントの違反応答耐性 単体5件**（EOF/切り詰め/ゴミ/GOAWAY/RST で panic・hang なし Err）。HTTP/3 上流のみ残件。F-53 子 |
| F-68 | P2 | 完了 | [features/F-68-resource-exhaustion-tests.md](features/F-68-resource-exhaustion-tests.md) | リソース枯渇テスト。io_uring SQ/CQ 飽和調査で **[B-24](bugs/B-24-sq-full-future-hang.md)（SQ 満杯時の I/O Future 永久ハング）を検出・修正**（回帰単体で実証）。`resource_exhaustion_chaos.sh` にメモリスイープ + 起動失敗/稼働中枯渇の切り分けを追加。F-53 子 |
| F-69 | P2 | 完了 | [features/F-69-pumba-network-kernel-chaos.md](features/F-69-pumba-network-kernel-chaos.md) | ネットワーク/カーネル層カオス（Pumba/tc netem）。loss/delay/dup/corrupt に加え **reorder + 複合(tc で loss+delay 同時)** を追加。F-53 子 |
| F-70 | P2 | 完了 | [features/F-70-wasm-abi-fuzzing.md](features/F-70-wasm-abi-fuzzing.md) | WASM モジュール/ABI 境界ファジング。`wasm_abi`（モジュールバイト列）+ **`wasm_host_abi`（ゲスト→ホスト ABI マップ復元境界の冪等性検査、`fuzz_api::wasm_host_abi_map_smoke`）** を追加。回帰単体テスト + container_security への opt-in 配線済み。実インスタンス化を伴う host functions ファジングのみ残件。F-52 子 |
| F-71 | P2 | 完了 | [features/F-71-asan-corpus-fuzzing.md](features/F-71-asan-corpus-fuzzing.md) | ASAN + **TSAN パイプライン**（`run_libfuzzer_tsan.sh`）+ **version-controlled 回帰 corpus（F-80）**。MSAN（-Zbuild-std 要）・外部永続化のみ残件。F-52 子 |
| F-72 | P3 | 完了 | [features/F-72-security-testing-further-hardening.md](features/F-72-security-testing-further-hardening.md) | セキュリティテスト追加提案（レポート範囲外）。6 項目を **個別チケット F-75〜F-80 へ分割**して backlog へ反映（本チケットの受け入れ条件を達成） |
| F-75 | P2 | 完了 | [features/F-75-secret-scan-gitleaks.md](features/F-75-secret-scan-gitleaks.md) | シークレットスキャン（gitleaks）。`run_gitleaks.sh` 追加（SARIF・redact・非ブロッキング）、`run.sh` フェーズ 4g + `report.sh` に配線。トリアージ方針の文書化が残件。F-72 子 |
| F-76 | P2 | 完了 | [features/F-76-http-smuggling-active-tests.md](features/F-76-http-smuggling-active-tests.md) | HTTP リクエストスマグリング能動テスト。`run_smuggling.sh`（CL.TE/TE.CL/複数CL/終端非chunked を 400 検証）+ Rust 単体/E2E。**実行中に B-23 を検出・修正**。H2C 専用プローブのみ残件。F-72 子 |
| F-77 | P3 | 完了 | [features/F-77-differential-testing.md](features/F-77-differential-testing.md) | プロトコル差分テスト。`run_differential.sh`（Veil vs nginx を同一バックエンドでフロントし応答差分比較、スマグリング厳格拒否は allowlist）。envoy 追加のみ残件。F-72 子 |
| F-78 | P3 | 完了 | [features/F-78-oss-fuzz-integration.md](features/F-78-oss-fuzz-integration.md) | OSS-Fuzz 連携。`tools/oss-fuzz/`（project.yaml/Dockerfile/build.sh/README、6 ターゲット + F-80 seed 添付）を用意。実走・上流申請のみ残件（外部インフラ）。F-72 子 |
| F-79 | P3 | 未着手 | [features/F-79-fuzz-coverage-llvm-cov.md](features/F-79-fuzz-coverage-llvm-cov.md) | カバレッジ計測の常設化（cargo llvm-cov、suite サマリ統合）。F-72 子 |
| F-80 | P2 | 完了 | [features/F-80-regression-corpus.md](features/F-80-regression-corpus.md) | 回帰コーパス固定。**`fuzz/regression_corpus/`（version-controlled）を新設**し B-21 クラッシュ seed を固定、3 fuzz ランナーが起動時にコーパスへ複製。B-21/B-22 は単体テスト固定済み。F-72 子 |
| F-81 | P2 | 未着手 | [features/F-81-sbom-ci-integration.md](features/F-81-sbom-ci-integration.md) | SBOMのCIパイプライン統合およびRelease添付。F-65から分離。 |
| F-82 | P2 | 未着手 | [features/F-82-fuzzing-ci-nightly.md](features/F-82-fuzzing-ci-nightly.md) | ファジングのCI統合（長時間実行・Corpus永続化）。F-52から分離。 |
| F-83 | P3 | 未着手 | [features/F-83-nuclei-landlock-firing.md](features/F-83-nuclei-landlock-firing.md) | Nuclei（DAST テンプレスキャン）と Landlock 違反の意図的発火コンテナテスト。F-54 から分離。 |
| F-84 | P1 | 完了 | [features/F-84-iouring-executor-cqe-fuzzing.md](features/F-84-iouring-executor-cqe-fuzzing.md) | io_uring executor 擬似 CQE 注入ファジング（異常 res・偽造/stale user_data・順序逆転で panic/ガード二重実行/スロットリークなし）。fuzz ターゲット `io_uring_executor` + 回帰単体。レポート提案1 |
| F-85 | P2 | 完了 | [features/F-85-e2e-chaos-sanitizers.md](features/F-85-e2e-chaos-sanitizers.md) | E2E カオス負荷への ASAN/TSAN 統合。`chaos/e2e_sanitizer_chaos.sh`（nightly + -Zbuild-std で sanitizer 計装 Veil をビルドし、カオス負荷 + SIGHUP 下で UAF/リーク/データ競合を実行検出。mimalloc 除外・cmake 導入）。ASAN 実行で findings=0 確認。レポート提案2 |
| F-86 | P2 | 完了 | [features/F-86-syscall-fault-injection-chaos.md](features/F-86-syscall-fault-injection-chaos.md) | syscall レベルのフォールトインジェクション。`chaos/syscall_chaos.sh`（strace inject で io_uring_enter に EBUSY/ENOMEM/EINTR、setup に EFAULT を注入、panic/segfault なしを検証）。EBUSY 注入で graceful-exit 確認。レポート提案3 |
| F-87 | P1 | 完了 | [features/F-87-future-cancellation-safety-tests.md](features/F-87-future-cancellation-safety-tests.md) | io_uring Future のランダム Drop（キャンセル安全性）テスト。`tests/runtime_cancellation_test.rs`（recv/send/accept/timer の提出前・in-flight・多重キャンセル Drop + liveness プローブ、決定的シード）。レポート提案4 |
| F-88 | P2 | 完了 | [features/F-88-ast-hotpath-blocking-lint.md](features/F-88-ast-hotpath-blocking-lint.md) | AST ベース静的解析（clippy disallowed-methods）でホットパスのブロッキング呼び出し混入を検出。**`clippy.toml` + 理由付き allow + CI `cargo clippy --features full --tests` + B-26 修正済み** |
| F-90 | P1 | 完了 | [features/F-90-container-security-full-features.md](features/F-90-container-security-full-features.md) | container_security full features 網羅（14 プローブ + http3/ws クライアント + テストケース文書 + 実行検証）。B-29〜B-30・B-32〜B-34 修正済み、B-31 は意図的設計で保留 |
| F-91 | P1 | 完了 | [features/F-91-http3-grpc-coverage.md](features/F-91-http3-grpc-coverage.md) | HTTP/3・gRPC の E2E/container_security 網羅。不足ケース実装済。失敗は B-38/B-39。正本: `docs/artifacts/required_test_cases.md` |
| F-92 | P1 | 完了 | [features/F-92-http3-grpc-detailed-coverage.md](features/F-92-http3-grpc-detailed-coverage.md) | http3_grpc_test_coverage_report 指摘の E2E 詳細5件 + gRPC Slowloris/RST flood + H3 handshake/amplification。B-40 修正済み |
| F-93 | P1 | 完了 | [features/F-93-http3-grpc-report-remaining.md](features/F-93-http3-grpc-report-remaining.md) | http3_grpc_test_coverage_report 残件: connection_reuse / early_data / gRPC over H3 streaming・metadata・error + QUIC gRPC 攻撃プローブ。B-41 修正済み |
| F-94 | P1 | 完了 | [features/F-94-http3-grpc-report-items-1-11.md](features/F-94-http3-grpc-report-items-1-11.md) | http3_grpc_test_coverage_report 項目1〜11（Alt-Svc・UDP fallback・gRPC FC/WASM・h3spec ハーネス・amplification/0-RTT/fragmented LPM/half-closed・fuzz）。**CI は F-95** |
| F-95 | P2 | 未着手 | [features/F-95-h3spec-ci-integration.md](features/F-95-h3spec-ci-integration.md) | h3spec の CI 組み込み（F-94 から分離。ハーネスは F-94、GHA 配線のみ本チケット）。**本作業では対象外** |
| F-96 | P1 | 完了 | [features/F-96-http3-grpc-edge-dos-coverage.md](features/F-96-http3-grpc-edge-dos-coverage.md) | http3_grpc_test_coverage_report §5: PMTU/CID/keepalive/GOAWAY + gRPC retry/PING/異常終了 + container_security DoS 8 件。**CI は F-95** |
| F-97 | P1 | 完了 | [features/F-97-http3-grpc-app-layer-coverage.md](features/F-97-http3-grpc-app-layer-coverage.md) | レポート §4: HTTP/3 アプリ層 4 + gRPC 高度 3 + container_security 5（Slowloris/QPACK 枯渇/Path Bypass/WASM Crash/Base64 DOS）。**CI は F-98** |
| F-98 | P2 | 未着手 | [features/F-98-http3-grpc-app-layer-ci.md](features/F-98-http3-grpc-app-layer-ci.md) | F-97 追加 E2E/プローブの CI 組み込み（F-97 から分離。**本作業では対象外**） |
| F-99 | P1 | 完了 | [features/F-99-test-coverage-report-h3-grpc.md](features/F-99-test-coverage-report-h3-grpc.md) | test_coverage_report: gRPC over H3 E2E 7 件 + H3 メトリクス + container_security H3 gRPC/gRPC-Web 攻撃 6 件。**CI は F-100** |
| F-100 | P2 | 未着手 | [features/F-100-test-coverage-report-ci.md](features/F-100-test-coverage-report-ci.md) | F-99 追加 E2E/プローブの CI 組み込み（F-99 から分離。**本作業では対象外**） |
| F-101 | P1 | 完了 | [features/F-101-http3-web-features-flow-control.md](features/F-101-http3-web-features-flow-control.md) | http3_grpc_test_coverage_report: H3 静的/リダイレクト/SNI・cert/oversized/Alt-Svc アップグレード E2E + QUIC フロー制御違反プローブ。**CI は F-102** |
| F-102 | P2 | 未着手 | [features/F-102-http3-web-features-ci.md](features/F-102-http3-web-features-ci.md) | F-101 追加 E2E/プローブの CI 組み込み（F-101 から分離。**本作業では対象外**） |
| F-103 | P1 | 完了 | [features/F-103-http3-grpc-edge-h3-coverage.md](features/F-103-http3-grpc-edge-h3-coverage.md) | レポート: gRPC over H3 エッジ E2E 12 + H3 multiplex coalesce 1 + container_security S-G-H3-09〜13 / S-H3-18〜20。**CI は F-104** |
| F-104 | P2 | 未着手 | [features/F-104-http3-grpc-edge-h3-ci.md](features/F-104-http3-grpc-edge-h3-ci.md) | F-103 追加 E2E/プローブの CI 組み込み（F-103 から分離。**本作業では対象外**） |
| F-73 | P1 | 完了 | [features/F-73-http2-send-zerocopy-writeall.md](features/F-73-http2-send-zerocopy-writeall.md) | HTTP/2 送信ホットパスの write_all ゼロコピー化（per-frame の 2 度目の to_vec 確保+コピーを排除）。A/B で **HTTP/2 +11.6%**（1577→1761 req/s、nginx 比 75%→84%）、HTTP/1.1 不変・応答ボディ sha256 一致。レポート `docs/artifacts/performance_report_veil_vs_nginx_v3.md` |
| F-74 | P1 | 完了 | [features/F-74-http2-send-frame-coalescing.md](features/F-74-http2-send-frame-coalescing.md) | HTTP/2 送信ホットパスのフレーム連結（HEADERS/DATA コアレッシング）。1 レスポンス分のフレームを接続再利用連結バッファ `write_buf`（スレッドローカルプール）へ積み **1 回の書き込み** で送出。`encode_*_into` 追記 API・`send_headers_buffered`・128KB 途中フラッシュ閾値を追加。per-frame 送信システムコールを削減。単体 660 / http2 E2E 11 / gRPC E2E 35 グリーン。F-73 続き |
| F-89 | P1 | 進行中 | [features/F-89-perf-full-features-coverage.md](features/F-89-perf-full-features-coverage.md) | perf ハーネスの full features 網羅拡充（`docs/artifacts/perf_measurement_report.md` 起点）。wasm/metrics/access-log/rate-limit/admin/otel/l4 の feat 構成 + パススルー WASM モジュール。http3/grpc/websocket 計測は残件 |
| F-11 | P3 | 未着手 | [features/dashboard.md](features/dashboard.md) | ダッシュボード機能 |
| F-12 | P3 | 未着手 | [features/config-generator-webui.md](features/config-generator-webui.md) | config.toml ジェネレータ Web UI |
| F-13 | P3 | 未着手 | [features/documentation-site.md](features/documentation-site.md) | 公式ドキュメントサイト |

### フェーズ 2（機能安定後）

| ID | 優先度 | 対応状況 | ドキュメント | 概要 |
|----|--------|----------|--------------|------|
| F-14 | P3 | 完了 | [features/post-stability-containerization.md](features/post-stability-containerization.md) | コンテナ化（`docker/` glibc/musl・seccomp・非 root。env 上書きは F-07 残件） |
| F-15 | P3 | 未着手 | [features/post-stability-aarch64.md](features/post-stability-aarch64.md) | aarch64 対応 |
| F-16 | P3 | 未着手 | [features/freebsd-support.md](features/freebsd-support.md) | FreeBSD 対応 (kqueue, kTLS, Capsicum 等) |
| F-17 | P3 | 未着手 | [features/openbsd-support.md](features/openbsd-support.md) | OpenBSD 対応 (pledge, unveil 等) |

---

## バグチケット一覧

（チケット発行時に行を追加する）

| ID | 優先度 | 対応状況 | ドキュメント | 概要 |
|----|--------|----------|--------------|------|
| B-01 | P1 | 完了 | [bugs/B-01-iouring-accept-nonblock.md](bugs/B-01-iouring-accept-nonblock.md) | io_uring accept が O_NONBLOCK を設定せず body timeout が発火しない |
| B-02 | P1 | 完了 | [bugs/B-02-408-connection-not-closed.md](bugs/B-02-408-connection-not-closed.md) | 408 送信後も接続を閉じず、クライアントが read タイムアウトまでブロック |
| B-03 | P1 | 完了 | [bugs/B-03-header-size-check-includes-body.md](bugs/B-03-header-size-check-includes-body.md) | ヘッダーサイズチェックにボディバイトが含まれ、正常リクエストが 431 で誤拒否される |
| B-04 | P1 | 完了 | [bugs/B-04-wasm-filter-missing-https-path.md](bugs/B-04-wasm-filter-missing-https-path.md) | WASM レスポンスフィルタが HTTPS バックエンドパスに未適用 |
| B-05 | P1 | 完了 | [bugs/B-05-wasm-modules-thread-local-race.md](bugs/B-05-wasm-modules-thread-local-race.md) | WASM モジュールリストの thread_local 競合により並行リクエストでフィルタが未適用になる |
| B-06 | P2 | 完了 | [bugs/B-06-grpc-h2c-trailer-not-forwarded.md](bugs/B-06-grpc-h2c-trailer-not-forwarded.md) | gRPC H2C レスポンストレーラーが HTTP/1.1 クライアントに転送されない |
| B-07 | P0 | 完了 | [bugs/B-07-iouring-future-drop-uaf.md](bugs/B-07-iouring-future-drop-uaf.md) | io_uring Future の Drop 未実装による UAF・タスク二重 poll（200 接続ストレスで segfault）→ 修正し segfault 消失 |
| B-08 | P0 | 完了 | [bugs/B-08-http2-read-buffer-corruption.md](bugs/B-08-http2-read-buffer-corruption.md) | HTTP/2 読み込みバッファ破損（部分フレーム時の offset0 上書き・返却 len 誤信）で H2C/gRPC が 502。B-07 修正で顕在化 → 修正し H2C 29/29 通過 |
| B-09 | P1 | 完了 | [bugs/B-09-l4-forward-writes-full-buffer.md](bugs/B-09-l4-forward-writes-full-buffer.md) | L4 forward_direction が読み取り n バイトでなくバッファ全長(64KB)を送信し転送破損（TLS パススルー不成立）。F-30 の L4 E2E 追加で発覚 → set_len(n) で修正 |
| B-10 | P2 | 完了 | [bugs/B-10-e2e-parallel-shared-state-flaky.md](bugs/B-10-e2e-parallel-shared-state-flaky.md) | E2E 並列実行でロードバランシング系テストが共有 Round Robin ステートと干渉しフレーキー化（専用プール `/rr-test/` へ隔離。cache/revalidation の単体テスト直列化も実施） |
| B-11 | P3 | 完了 | [bugs/B-11-expect-100-continue-intermittent-hang.md](bugs/B-11-expect-100-continue-intermittent-hang.md) | Expect: 100-continue の POST が間欠的にハング。根本原因は Expect のバックエンド転送 × 応答パーサの 1xx 中間応答未対応 → Expect をプロキシで終端 + 1xx 読み捨て（drain_interim_responses）で修正（20 回連続成功・curl 実フロー 60/60） |
| B-12 | P3 | 完了 | [bugs/B-12-http3-request-body-streaming-stall.md](bugs/B-12-http3-request-body-streaming-stall.md) | HTTP/3 リクエストボディストリーミングが間欠的にストール。根本原因は h3 クライアントの fin 直前 GREASE フレーム × 「h3.poll をパケット受信時のみ実行」の組み合わせで Finished イベントが永久滞留する設計バグ → poll を毎イテレーション実行 + pump に stream_finished 直接確認で修正（20 回連続成功） |
| B-13 | P1 | 完了 | [bugs/B-13-seccomp-faccessat2-static-404.md](bugs/B-13-seccomp-faccessat2-static-404.md) | seccomp 許可リストに `faccessat2`(439) が無く、seccomp 有効時に静的ファイル配信が 404（HTTP/1.1 全滅・musl 版配信不能の一因）。`ALLOWED_SYSCALLS` と docker seccomp.json に faccessat/faccessat2 を追加 |
| B-14 | P1 | 完了 | [bugs/B-14-nocache-static-file-404.md](bugs/B-14-nocache-static-file-404.md) | `cache` feature 無効（default features 等）で静的ファイル配信が 404。`get_file_info` スタブが `None` を返していた → キャッシュせず実解決する実装へ修正 |
| B-15 | P1 | 完了 | [bugs/B-15-dockerfile-fuzz-workspace-build.md](bugs/B-15-dockerfile-fuzz-workspace-build.md) | Dockerfile(glibc/musl) の cacher が fuzz ワークスペースメンバ未対応でビルド失敗（exit 101）→ cacher で fuzz マニフェスト+スタブを用意 |
| B-16 | P0 | 完了 | [bugs/B-16-splice-pipe-refcell-borrow-panic.md](bugs/B-16-splice-pipe-refcell-borrow-panic.md) | kTLS splice パイプ取得（`get_splice_pipe` の `borrow_mut`）で RefCell 二重借用 panic。**修正済み**: `Ref` 返却＋`'static` transmute を廃止し、L4（F-40）と同じ checkout/return 型プール（`PooledSplicePipe` RAII ガード + FIONREAD 残データ検査つき返却）へ変更。回帰単体テスト 4 件追加 |
| B-17 | P1 | 完了 | [bugs/B-17-malformed-backend-client-hang.md](bugs/B-17-malformed-backend-client-hang.md) | 不正バックエンド応答でクライアント可視のハング。**修正済み**: ヘッダーフェーズ失敗の即時 502/504 送出、`BACKEND_HEADER_TIMEOUT`(10s)・`MAX_RESPONSE_HEADER_SIZE`(64KB) 新設、CL 未達/chunked 終端前 EOF の即時クローズ（`client_must_close` 伝搬）。E2E 回帰テスト 8 件（`test_b17_*`）追加 |
| B-18 | P2 | 完了 | [bugs/B-18-http3-gso-batch-emsgsize-overflow.md](bugs/B-18-http3-gso-batch-emsgsize-overflow.md) | HTTP/3 GSO バッチが sendmsg の UDP ペイロード上限（65507B）を超え EMSGSIZE でバッチ全体（最大 64 パケット）が破棄され得る。F-60 実装中に検出。**修正済み**: `MAX_GSO_BATCH_BYTES` による追加前 flush + flush 判定の純関数化・単体テスト |
| B-19 | P1 | 完了 | [bugs/B-19-proxy-wasm-abi-mismatch.md](bugs/B-19-proxy-wasm-abi-mismatch.md) | Proxy-Wasm ABI 不一致（BufferType 番号・マップ直列化形式）で SDK の読み取り系 API を使うモジュールが panic。F-62 で検出。**修正済み**: 定数を SDK/ABI 準拠へ、直列化を `host/abi.rs` へ集約（SDK 互換ワイヤ形式 + 不正データ拒否） |
| B-20 | P1 | 完了 | [bugs/B-20-wasm-sync-call-async-store-panic.md](bugs/B-20-wasm-sync-call-async-store-panic.md) | WASM 読み取り系ホスト関数 5 つが async store で同期 `call` を使い panic（"must use call_async"）。F-62 で検出。**修正済み**: `func_wrap_async` + `call_async` 化 |
| B-21 | P1 | 完了 | [bugs/B-21-hpack-huffman-decode-shift-panic.md](bugs/B-21-hpack-huffman-decode-shift-panic.md) | HPACK Huffman デコーダが不正入力（符号非一致でビット蓄積）でシフト量オーバーフロー panic。外部から HTTP/2 ヘッダで到達可能な DoS 面。`cargo fuzz`（F-52）で検出。**修正済み**: 最長符号長(30bit)超過で HuffmanDecodeError を返すガード + 回帰テスト（クラッシュ入力・ラウンドトリップ） |
| B-22 | P2 | 完了 | [bugs/B-22-path-wildcard-boundary-mismatch.md](bugs/B-22-path-wildcard-boundary-mismatch.md) | パスワイルドカード `/api/*` が境界パス `/api`・`/api/` を取りこぼす（matchit キャッチオール `{*rest}` が空セグメント非対応、fallback `matches_pattern` と意味論不一致）。F-56 プロパティテストで検出。**修正済み**: ワイルドカードを matchit + fallback へ二重登録し境界意味論を一致（候補は上位で dedup）。回帰単体 + プロパティテスト |
| B-23 | P1 | 完了 | [bugs/B-23-request-smuggling-cl-te.md](bugs/B-23-request-smuggling-cl-te.md) | HTTP リクエストスマグリング（CL.TE）。`Content-Length: 0` + `Transfer-Encoding: chunked` が拒否されずバックエンドへ CL+TE 曖昧メッセージを転送（デシンク）。F-76 プローブ設計中に検出。**修正済み**: `classify_request_framing` 純関数で一律 400 拒否 + chunked 時 CL 転送除去。単体6+E2E2 |
| B-24 | P1 | 完了 | [bugs/B-24-sq-full-future-hang.md](bugs/B-24-sq-full-future-hang.md) | io_uring SQ リング満杯時に全 I/O Future が SQE 未投入のまま `submitted=true` にして永久ハング（CQ 永久未着）。F-68 リソース枯渇テスト設計中に検出。**修正済み**: `get_sqe_or_submit`（満杯時 pending 提出→再取得）+ 確保失敗を WouldBlock で graceful 化。回帰単体1 |
| B-25 | P1 | 完了 | [bugs/B-25-reverse-proxy-http1-wrk-zero-completed.md](bugs/B-25-reverse-proxy-http1-wrk-zero-completed.md) | 逆プロキシ構成 `feat_proxy` の HTTP/1.1（kTLS 有効）が wrk で「完了リクエスト0」。**原因確定・修正済み**: `runtime/splice.rs` が全 splice に `SPLICE_F_MORE` を無条件付与し、kTLS の 16KiB 未満の最終部分 TLS レコードがカーネル内に保留され応答が完結しなかった。既定 `splice()` から MORE を除去し、後続データが確実な中間チャンク専用の `splice_more()` を新設（curl 全量受信・wrk 正常計上・perf 再計測で検証） |
| B-26 | P1 | 完了 | [bugs/B-26-sync-fs-on-event-loop.md](bugs/B-26-sync-fs-on-event-loop.md) | イベントループ上の同期 FS 呼び出し残存（HTTP/3 sendfile の whole-file read・runtime::io::read/remove_file・ディスクキャッシュ async_io）。**F-88 の clippy disallowed-methods で検出・修正済み**: 3 系統とも runtime::offload へ退避 |
| B-27 | P1 | 完了 | [bugs/B-27-ktls-http2-short-write-frame-desync.md](bugs/B-27-ktls-http2-short-write-frame-desync.md) | kTLS + HTTP/2 高並行送信が FRAME_SIZE_ERROR GOAWAY で激減（`h2_1_ktls_1_lb_kernel_*` 256〜736 req/s）。**原因確定・修正済み**: `runtime/io.rs` の `write_all` が short write の残りを書かず WriteZero を返し、送信済みプレフィックスでフレーム同期が破壊されていた。`SlicedIoBuf` による継続書き込みへ修正（回帰単体 3 件 + h2load 検証） |
| B-28 | P1 | 完了 | [bugs/B-28-h2-proxy-no-backend-pooling-port-exhaustion.md](bugs/B-28-h2-proxy-no-backend-pooling-port-exhaustion.md) | HTTP/2 逆プロキシがバックエンド接続をリクエスト毎に新規作成・クローズし、TIME_WAIT 蓄積でエフェメラルポート枯渇（EADDRNOTAVAIL → 502、h2load 30000 リクエスト中 1000〜1500 件 5xx）。**修正済み**: `relay_h2_response` に再利用可否判定を追加し、HTTP/1.1 経路と同じ `HTTP_POOL`/`HTTPS_POOL` で接続を再利用（CL 全量消費 + 非 close 時のみ返却）。chunked 応答と H2C バックエンドの再利用は残件 |
| B-29 | P1 | 完了 | [bugs/B-29-admin-api-http2-unreachable.md](bugs/B-29-admin-api-http2-unreachable.md) | 管理 API が HTTP/2 経路で 404（HTTP/1.1 のみ 401/200）。**修正済み**: `handle_http2_admin_request` を HTTP/2 単一リクエスト経路へ配線。F-90 admin_security_probe 通過 |
| B-30 | P1 | 完了 | [bugs/B-30-wasm-filter-http2-file-missing.md](bugs/B-30-wasm-filter-http2-file-missing.md) | WASM フィルタが HTTP/2 File 応答に未適用。**修正済み**: `apply_h2_wasm_response_headers` を File 応答へ適用。F-90 wasm_security_probe 通過 |
| B-31 | P2 | 保留 | [bugs/B-31-rate-limit-thread-local-per-worker.md](bugs/B-31-rate-limit-thread-local-per-worker.md) | レートリミットが thread_local でワーカー分散（**意図的設計**。グローバル集約は却下、`RATE_LIMITER` thread_local を維持） |
| B-32 | P2 | 完了 | [bugs/B-32-compression-not-applied-http2.md](bugs/B-32-compression-not-applied-http2.md) | HTTP/2 で Accept-Encoding 時も Content-Encoding 未付与。**修正済み**: `build_h2_compressed_file_response` + ルート圧縮設定の伝搬。F-90 compression_cache_probe 通過 |
| B-33 | P2 | 完了 | [bugs/B-33-l4-listener-upstream-dns-startup.md](bugs/B-33-l4-listener-upstream-dns-startup.md) | L4 リスナーが上流 DNS 未解決で起動失敗。**修正済み**: `L4UpstreamTarget` + `resolve_upstream_target`（offload 遅延解決）。F-90 l4_flood_probe 通過 |
| B-34 | P2 | 完了 | [bugs/B-34-http3-quiche-client-response-timeout.md](bugs/B-34-http3-quiche-client-response-timeout.md) | HTTP/3 quiche クライアント応答タイムアウト。**修正済み**: クライアント `initial_max_streams_uni` 設定 + HTTP/3 単一ワーカー + h3 早期初期化。F-90 http3_probe 通過 |
| B-35 | P1 | 完了 | [bugs/B-35-http2-upstream-tls-insecure-ignored.md](bugs/B-35-http2-upstream-tls-insecure-ignored.md) | HTTP/2 上流 HTTPS で `tls_insecure` 無視 → 自己署名バックエンドへ 502（UnknownIssuer）。E2E 214 件連鎖失敗の根本原因。**修正済み**: `handle_http2_proxy_https` + H2 ストリーミング経路で `get_tls_connector_insecure()` を使用 |
| B-36 | P2 | 完了 | [bugs/B-36-veil-tls-insecure-overrides-upstream-verify.md](bugs/B-36-veil-tls-insecure-overrides-upstream-verify.md) | `VEIL_TLS_INSECURE=1` が per-upstream `tls_insecure=false` を上書き。**修正済み**: 上流経路から env OR を削除し per-upstream 設定のみに |
| B-37 | P2 | 完了 | [bugs/B-37-l4-tls-terminate-e2e-timeout.md](bugs/B-37-l4-tls-terminate-e2e-timeout.md) | L4 `tls=terminate` リスナー（8446）が E2E でタイムアウト。**修正済み**: `src/l4/proxy.rs` に TLS 終端 + 平文転送を実装 |
| B-38 | P1 | 完了 | [bugs/B-38-http3-wasm-response-headers-not-applied.md](bugs/B-38-http3-wasm-response-headers-not-applied.md) | HTTP/3 WASM レスポンスヘッダ未適用 → `apply_h3_wasm_response_headers` で修正。`test_http3_wasm_integration` PASS |
| B-39 | P1 | 完了 | [bugs/B-39-http3-grpc-proxy-502.md](bugs/B-39-http3-grpc-proxy-502.md) | HTTP/3 gRPC 502 → H2C 上流 + フルパス保持で修正。`test_grpc_over_http3` PASS |
| B-40 | P1 | 完了 | [bugs/B-40-grpc-path-prefix-stripping.md](bugs/B-40-grpc-path-prefix-stripping.md) | H1/H2 gRPC が `/*` プレフィックス除去で UNIMPLEMENTED/502。フルパス保持 + H2 use_h2c + HPACK 小文字化で修正（F-92） |
| B-41 | P1 | 完了 | [bugs/B-41-http3-grpc-body-trailers-hang.md](bugs/B-41-http3-grpc-body-trailers-hang.md) | HTTP/3 gRPC ボディあり応答が trailers API 誤用でハング → `send_additional_headers` で修正（F-93） |

---

## メタ

- 実装・仕様変更時は [AGENTS.md](../../AGENTS.md) と README の更新を同じ変更単位で行う。
- AI が生成する作業ログ・レポートは [AGENTS.md](../../AGENTS.md) の **「AI 成果物・ログ・一時ファイル」** に従い **`docs/artifacts/`** に置く（本バックログの個別 md は **仕様・チケット用**）。
