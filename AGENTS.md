# AGENTS.md — Veil (veil-proxy)

AI エージェントおよびコントリビュータ向けの **最小指針**。機能説明・ビルド例・テスト手順の **正** は [README.md](README.md) / [docs/readme/README.ja.md](docs/readme/README.ja.md)。フィーチャー定義は [Cargo.toml](Cargo.toml) の `[features]`。

---

## プロジェクトの事実

- クレート名 `veil`（ディレクトリ名 `veil-proxy`）。リリースバイナリは `target/release/veil`。
- **ライブラリ + バイナリ構成**。mod 宣言・公開 API は [src/lib.rs](src/lib.rs)（`cargo fuzz`・統合テスト向けに公開）、サーバ起動配線は [src/entry.rs](src/entry.rs)（`veil::run()`）、[src/main.rs](src/main.rs) は `veil::run()` を呼ぶだけの薄いエントリ。
- 設定は TOML（`serde`）。ホットリロード・検証の挙動は README を参照。

---

## 設計哲学・こだわりポイント

**Rust の安全性を土台に、Linux カーネル（io_uring、kTLS、seccomp、Landlock、ソケット/CBPF）とユーザー空間を噛み合わせ、HTTP/1.1・2・3 のデータプレーンを tokio/monoio なしで高スループット化・ゼロコピー化。io_uring は `src/runtime/` の独自実装（libc + bytes クレートのみ使用）を通じて直接操作する。開発効率を度外視して最大限の性能を目指し、かつ運用で効く動的設定・観測・拡張（Proxy-Wasm）まで載せる。**

変更やレビューでは、個別機能だけでなく **ここに反しないか** を意識する。

---

## ホットパス絶対規則（最優先・例外なし）

データプレーン（接続受理〜リクエスト/レスポンス転送〜TLS/HTTP/2/HTTP/3/WASM 実行〜バックエンド I/O の各経路。1 リクエスト/1 コネクションあたり実行される全コード）では、次を **例外なく** 守る。レビュー時もこの観点を最優先で確認する。

- **同期処理（ブロッキング呼び出し）の使用を一切禁止する。** ホットパスのあらゆる I/O・待機・システムコールは **必ず非同期**（`src/runtime/` の io_uring 非同期 API、`.await`）で行う。`std::net`・ブロッキング `libc::read/write/connect`・同期 DNS 解決・`block_on`・`std::thread::sleep`・同期ロック待ち等をホットパスに置いてはならない。WASM 実行のような CPU バウンド処理も、ワーカースレッドを占有しないよう非同期（協調的 yield）で実行する。対応する io_uring オペコードが存在しないブロッキング処理（例: シンボリックリンク解決を伴う `canonicalize`）は `src/runtime/offload.rs` の `offload()`（専用スレッドプール + スレッドごと eventfd の POLL_ADD で完了待機）でワーカースレッドへ退避し、**イベントループ自体は決してブロックしない**こと（新規 io_uring オペコードを増やしてセキュリティサーフェスを広げてはならない）。
- **メモリアロケーションは、パフォーマンス上必要である場合を除いて一切禁止する。** リクエストごとの `Vec`/`String`/`HashMap`/`Box` 等の新規確保、`to_vec()`/`to_string()`/`clone()`（ディープコピー）/`format!`/`collect()` をホットパスで増やさない。
- **ゼロコピーを徹底する。** バッファは `bytes` クレート（`Bytes`/`BytesMut`、参照カウントによる共有・`split()`/`freeze()` によるゼロコピー分割）、`src/pool.rs` のスレッドローカルバッファプール、`splice(2)`/`sendfile(2)` 等のカーネルゼロコピー機構を用い、アロケーションとコピーを発生させない実装にする。
- **難易度や保守性を理由に妥協しない。** 実装・設計の難易度が高い場合でも一切妥協せず、保守性や実装難易度は度外視して、**最高性能のパフォーマンスとセキュリティ** を最優先に設計・実装する。
- 既存コードを変更・追加する際は「これはホットパスか？」を常に自問し、ホットパスなら上記をすべて満たすこと。満たせない設計なら設計自体をやり直す。

## 設計制約（要約）

変更時は **上記の設計哲学・ホットパス絶対規則** および次の箇条書きに反しないか確認する。

- **データプレーンは tokio / monoio に依存しない**（テスト・クライアント用途の tokio は別）。ランタイムは `src/runtime/` の独自実装を使用する。
- **`cfg(feature = "...")` を壊さない** — `default = []` のまま、無効 feature でもコンパイル可能に保つ。
- **ランタイムバックエンドは build.rs 発行の cfg で切替（F-120/F-125/F-127）** — デフォルトは Linux io_uring（`veil_rt_uring` = `src/runtime/uring/`、`default` 不変・性能非劣化）。`--features epoll`（Linux）と BSD（FreeBSD/OpenBSD）・**macOS** は readiness reactor（`veil_rt_reactor` = `src/runtime/reactor/`、poller は `veil_poller_epoll` / `veil_poller_kqueue`。macOS は BSD と同じ kqueue poller を再利用）。Windows は `veil_poller_wsapoll` の cfg 名のみ発行済み（実装は未着手）。FreeBSD は追加で `--features aio`（`veil_aio`）により `TcpStream::read`/`write` を POSIX AIO（`aio_read`/`aio_write` + `EVFILT_AIO` 完了通知）へ切替可能（既定オフ、`src/runtime/reactor/aio.rs`。既存 kqueue readiness 経路は無改変・ゼロコストで維持）。**io_uring パスのロジックは変えない**（reactor 追加でも uring 生成コードを等価に保つ）。公開パス `runtime::tcp` 等はファサード re-export で不変に保つ。
- **プラットフォーム別セキュリティ／kTLS は `target_os` で分岐**（F-120/F-125/F-126） — Linux: seccomp（バックエンド別に許可 syscall 分割・最小権限）/Landlock/CBPF、kTLS（`veil_ktls` = `feature="ktls"` かつ linux/freebsd。Linux 経路は `src/ktls.rs`/`src/ktls_rustls.rs`、FreeBSD 経路は `src/ktls_freebsd.rs` に完全分離し Linux ロジックは無変更）。FreeBSD: capsicum（cap_rights_limit / cap_enter / jail）+ kTLS 対応（`TCP_TXTLS_ENABLE`/`TCP_RXTLS_ENABLE`、F-126）。**capability mode（`cap_enter`）下の静的配信は dirfd + `openat`/`fstatat` + `O_RESOLVE_BENEATH` 相対化で完全動作（F-123）**：`cap_enter` 前に File ルート dirfd を確保（`security.rs` `init_static_dirfds`）し、`OpenOptions` を単一チョークポイントとして読み取り専用 open を dirfd 相対 `openat` へ、`get_file_info`/`load_backend` の stat を `fstatat` へ切替。cap-mode では `std::thread::sleep`（内部 `clock_nanosleep(CLOCK_MONOTONIC)`）が `ECAPMODE` で panic するため、背景常駐スレッドは `select(2)` ベースの `server::cap_safe_sleep` を使う（非 FreeBSD は std sleep 委譲・挙動不変）。OpenBSD: pledge / unveil（kTLS 非対応、simple_tls フォールバック）。**macOS**: `sandbox_init`（Seatbelt、`src/security.rs` の `macos_sandbox` モジュール。実機検証不可のため保守的な deny-default + 書き込みのみ制限プロファイル、kTLS 非対応・simple_tls フォールバック）。**rustls 暗号プロバイダも `target_os`＋`target_arch` で分岐**（F-122、`src/tls_provider.rs` と Cargo.toml の target 別依存を一致させる）: Linux/FreeBSD = `aws_lc_rs`（kTLS/quiche と AWS-LC 共有）、OpenBSD/macOS/x86_64-Windows = `ring`、**aarch64-Windows = `aws_lc_rs`**（ARM asm・NASM 不要で cmake クロスビルド可。ring 0.17 は aarch64-windows の prebuilt asm を持たず cargo-xwin でビルド不能なため arch 分割）。非対象 OS 用の設定キーは受理し警告して無視する。README の前提と矛盾させない。
- **ホットパス**でヒープ割り当て・不要なロック・コピー・同期呼び出しを増やさない（詳細は上の **ホットパス絶対規則**）。
- **HTTP/3 UDP データプレーンは io_uring パイプライン化済み（F-130）** — 受信は `runtime::uring::udp_recv::PipelinedUdpRecv`（`[http3].mmsg_batch_size` 本の `IORING_OP_RECVMSG` を常時 in-flight に保つソフトウェアパイプライン。1 回の `recv_batch()` 完了で複数データグラムをまとめて拾い、消費後は `rearm_ready()` で 1 回の submit にまとめて再投入）、送信は `runtime::uring::udp_send::UringUdpSend`（`IORING_OP_SENDMSG` を GSO `UDP_SEGMENT` cmsg 付きで複数 SQE 同時 submit）。ホットパスに libc `recvmmsg`/`sendmmsg` は登場しない。**真の `IORING_RECV_MULTISHOT` + provided buffers/buffer ring（C2）は未実装**（unconnected multi-peer UDP のアドレス安全性・ENOBUFS 耐性の課題が残るため見送り。`executor.rs` の `alloc_multishot_op`/`take_multishot_cqe` は将来 C2 用に未使用のまま残置）。`recvmmsg`/`sendmmsg` は DNS 解決と `VEIL_H3_MULTISHOT=0`/reactor ビルド時のフォールバック経路にのみ残る。
- **動的設定**は ArcSwap とリロード経路の不変条件を維持する。
- **`unsafe` は最小限** — 拡大時は不変条件をコメントで明示。

---

## 行動指針

1. 上記 **設計哲学・設計制約** に整合するか確認する。
2. 変更前に、触るコードの `cfg(feature)`、エラーハンドリング、ftlog、serde設定型を読む。
3. 外部契約（設定キー、CLI、メトリクス名、プロトコル範囲）を変えたら **同じ PR で README（必要なら .ja）を更新**する。`specs/` 等を使う場合も矛盾を残さない。
4. 大きなロジックは **専用モジュール**へ。`entry.rs` は配線中心に保つ（`main.rs` は `veil::run()` のみ）。
5. 挙動変更には **単体 / 統合 / E2E** のいずれかを追加または更新し、**`cargo test` で実証**する。

### AI 成果物・ログ・一時ファイル

評価レポート、調査メモ、セッションログ、スクラッチなどは **`docs/artifacts/` にのみ** 置く（他に散乱させない）。無ければ作成してよい。

---

## コーディング規約

- 既存に合わせる（多くは **日本語のモジュール／doc コメント**）。英語への統一リファクタはしない。
- `cargo fmt`、原則 `cargo clippy`。`#[allow(clippy::…)]` を同等の論拠なく増やさない。
- **ホットパスのブロッキング検出（[clippy.toml](clippy.toml) の `disallowed-methods`）**: 同期 `std::fs`・`std::thread::sleep`・ブロッキング `std::net` はデータプレーンで clippy エラー。正当な利用（`runtime::offload` 閉包内・専用スレッド・起動/リロードのコールドパス・テスト/ベンチ）は **理由コメント付きの個別 `#[allow(clippy::disallowed_methods)]`** で明示する（理由なしの追加は禁止）。
- 依存追加は慎重に。[Cargo.toml](Cargo.toml) の記法と feature を尊重。`ftlog` のレベル・頻度は既存に揃える。

---

## 作業フロー

- **1 タスク = 小粒度**（1 PR 1 目的）。
- feature 変更時は `cargo build --no-default-features` および **関連 feature の組み合わせ**で確認。
- プロキシ全体に触れる場合は [tests/e2e_setup.sh](tests/e2e_setup.sh) を参照。

### バックログ

- **機能追加**: [docs/backlog/features/](docs/backlog/features/) に 1 チケット 1 md（機能説明・改修内容・改修案など）。
- **バグ**: [docs/backlog/bugs/](docs/backlog/bugs/) に 1 件 1 md（事象・調査・改修案）。
- **親ドキュメント**: [docs/backlog/backlog.md](docs/backlog/backlog.md) に一覧・優先度・対応状況。チケットの **追加・ステータス変更時は必ず同じ変更で更新**する。

---

## 禁止事項

- 依頼範囲外のドライブバイリファクタ、無関係ファイルの変更。
- README / 仕様を更新せず挙動・設定だけ変えること。
- `default = ["ktls", "http2", "mimalloc"]` を崩して重い依存をデフォルト必須にすること。
- 検証なしの `unsafe` 拡大、安易な `#[ignore]`（やむを得ない場合は理由を文書化）。
- `docs/artifacts/` 以外への AI 専用成果物の散乱。
- バックログの個別 md だけ更新して [docs/backlog/backlog.md](docs/backlog/backlog.md) を更新しないこと。

---

## ビルド・テスト（入り口）

詳細・feature 組み合わせ・E2E・ベンチは **README の Build / Testing 節**を参照。

### 注意事項
- **コンパイル時の依存関係**: `entry.rs`/各モジュールは `http2` や `grpc`（もしくは `grpc-full`）feature が有効でないと、`send_grpc_trailers` の呼び出し箇所等でコンパイルエラーが発生します。そのため、ビルドやテストの際は必ず十分な feature（例：`--features "http2,grpc-full"` またはフルフィーチャー）を指定して実行してください。
- **E2Eテスト**: E2Eテストは専用のバックエンド環境を起動する必要があります。手動で直接 `cargo test` を叩くとバックエンドへの接続ができずタイムアウトするため、必ず `./tests/e2e_setup.sh test` を使用して自動セットアップ・実行・クリーンアップを行ってください。また、ポート競合エラーが発生した場合は、`pkill -f veil` 等で残存プロセスを終了させてから再実行してください。

### 実行コマンド例

```bash
# フル機能でのビルド
cargo build --features "full"

# E2Eテストの実行（自動セットアップ・クリーンアップ付き）
./tests/e2e_setup.sh test

# ユニットテストや統合テストの実行（features指定必須）
cargo test --bins --test integration_tests --features "full"
```

---

## ディレクトリ要約

| パス | 役割 |
|------|------|
| `src/main.rs` | 薄いバイナリエントリ（`veil::run()` を呼ぶだけ） |
| `src/lib.rs` | クレートルート・mod 宣言・公開 API（`cargo fuzz`・統合テスト向け） |
| `src/entry.rs` | サーバ起動配線（`run()`：ワーカースレッド・accept ループなど） |
| `src/runtime/` | 独自ランタイム。共有（buf.rs/io.rs/offload.rs）+ `uring/`（io_uring: ring/executor/tcp/timer/splice、`veil_rt_uring`。HTTP/3 UDP はパイプライン化 `IORING_OP_RECVMSG`（`udp_recv.rs` の `PipelinedUdpRecv`）/ `IORING_OP_SENDMSG`（`udp_send.rs` の `UringUdpSend`）= F-130）+ `reactor/`（epoll/kqueue readiness: poller/epoll/kqueue/executor/tcp/timer/splice、`veil_rt_reactor`）。バックエンドは build.rs 発行 cfg で選択、公開パスはファサードで不変（F-120） |
| `tests/`、`benches/` | 統合・E2E・ベンチ（`cargo test` が拾うホワイトボックス） |
| `examples/config.toml` | 設定リファレンス（全キー網羅・`src/config.rs` 同期） |
| `docs/readme/` | 日本語 README（`README.ja.md`） |
| `docs/artifacts/` | AI 成果物・一時ファイル |
| `docs/backlog/` | 機能・バグチケット（親は `backlog.md`） |
| `docker/` | コンテナイメージ（glibc/musl）・共有アセット（`assets/`：ssl/www/seccomp/Landlock） |
| `tools/` | Docker ベースの外形検証ツール。`tools/perf/` は glibc/musl/nginx 比較のパフォーマンス計測ハーネス（`gen_configs.sh` で 2⁴=16 直交表 + full features 機能ショーケース `feat_*` 構成 + **全プロトコル×全機能マトリクス（F-114: `h2_1_proxy_*`/`h3_file_*`/`h3_proxy*`/`grpc_h2_*`/`grpc_h3*`）** を生成 /`run_perf.sh` で反復計測（`CONFIG_GLOB` で scoped 化・gRPC over H3 はクライアント非対応で NA フェイルセーフ・TSV 行順は nginx → veil_glibc → veil_musl を保証、L4 構成は平文 9080 の readiness も確認 = F-118） /`analyze_results.sh` で median±stdev 集計。HTTP/1.1=wrk・HTTP/2=h2load・HTTP/3=QUIC 対応 h2load(`h2load-http3/`)・gRPC/WebSocket=grafana k6(`k6/`) の各クライアントで計測）、`tools/container_security/` はファジング・カオス・h2spec・セキュリティスキャンのオーケストレータ（`run.sh`）。**`tools/qemu/` は他アーキ/他 OS の実カーネル上でのビルド・E2E・perf 検証**（Docker ヘルパ経由の full-system QEMU）: Linux aarch64 io_uring（`aarch64-vm.sh`+`run-e2e-aarch64.sh`。KVM 不可ホストでは TCG が実用不能）、**FreeBSD arm64 のネイティブビルド + 動作確認**（`fbsd-arm64-vm.sh`。FreeBSD arm64 は Linux aarch64 と違い TCG でも実用起動。single-user provision・growfs・`pkg install rust cmake llvm` で in-VM ビルド）。詳細は `tools/qemu/README.md`。計測結果は `docs/perf/`（サマリ + 計測履歴は `docs/perf/README.md`、生データは `docs/perf/results_raw.tsv` = tools/perf 出力のコピー。詳細な生ログは
`docs/artifacts/perf_reports/` に git 管理外で保持）。F-121 の HPACK Huffman デコード LUT はコミット済みの `src/http2/hpack/huffman_decode_table.rs` をビルドが直接使用する。再生成用スクリプトは git 非管理の `docs/artifacts/gen_huffman_decode_table.py`（正本は `HUFFMAN_ENCODE_TABLE`）。通常のビルド・テスト・CI に Python 生成は不要 |

細かいモジュール対応は `src/lib.rs` の `mod` と README の構成を参照。
