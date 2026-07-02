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
| F-03 | P1 | 完了 | [features/tls-cert-zero-downtime.md](features/tls-cert-zero-downtime.md) | 0 ダウンタイム TLS 証明書更新 |
| F-04 | P1 | 未着手 | [features/vds-xds-dynamic-config.md](features/vds-xds-dynamic-config.md) | 動的設定配信 API（VDS / xDS 相当） |
| F-06 | P1 | 完了 | [features/resilience-outlier-detection.md](features/resilience-outlier-detection.md) | サーキットブレーカー・リトライ・異常検知 |
| F-09 | P1 | 完了 | [features/prometheus-feature-flags.md](features/prometheus-feature-flags.md) | Prometheus 拡充と feature 無効化 |
| F-01 | P2 | 完了 | [features/grpc.md](features/grpc.md) | gRPC / gRPC-Web の完成度・テスト拡充 |
| F-05 | P2 | 未着手 | [features/acme.md](features/acme.md) | ACME 統合 |
| F-07 | P2 | 進行中 | [features/fuzzing-chaos-security.md](features/fuzzing-chaos-security.md) | ファジング・カオス・h2spec・セキュリティスキャン（`tests/container_security/` 基盤完了。F-52〜F-57 で拡充中） |
| F-52 | P1 | 進行中 | [features/F-52-cargo-fuzz-libfuzzer.md](features/F-52-cargo-fuzz-libfuzzer.md) | cargo-fuzz / LibFuzzer（HPACK・config TOML ターゲット、`src/lib.rs`） |
| F-53 | P1 | 進行中 | [features/F-53-chaos-engineering-expansion.md](features/F-53-chaos-engineering-expansion.md) | カオス拡充（Toxiproxy 遅延注入、Pumba/tc 残件） |
| F-54 | P1 | 進行中 | [features/F-54-security-scan-expansion.md](features/F-54-security-scan-expansion.md) | セキュリティスキャン拡充（cargo-audit、testssl、ZAP 残件） |
| F-55 | P2 | 進行中 | [features/F-55-harness-hardening.md](features/F-55-harness-hardening.md) | ハーネス堅牢化（ポーリング、chaos タイムアウト、カーネルガード） |
| F-56 | P2 | 未着手 | [features/F-56-property-load-tests.md](features/F-56-property-load-tests.md) | プロパティベース・負荷テスト（proptest、wrk/k6） |
| F-57 | P2 | 未着手 | [features/F-57-container-security-ci.md](features/F-57-container-security-ci.md) | container_security CI/CD 統合（GitHub Actions nightly） |
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

---

## メタ

- 実装・仕様変更時は [AGENTS.md](../../AGENTS.md) と README の更新を同じ変更単位で行う。
- AI が生成する作業ログ・レポートは [AGENTS.md](../../AGENTS.md) の **「AI 成果物・ログ・一時ファイル」** に従い **`docs/artifacts/`** に置く（本バックログの個別 md は **仕様・チケット用**）。
