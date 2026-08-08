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
- **ランタイムバックエンドは build.rs 発行の cfg で切替（F-120/F-125/F-127/F-140）** — デフォルトは Linux io_uring（`veil_rt_uring` = `src/runtime/uring/`、`default` 不変・性能非劣化）。`--features epoll`（Linux）と BSD（FreeBSD/OpenBSD/**NetBSD**）・**macOS** は readiness reactor（`veil_rt_reactor` = `src/runtime/reactor/`、poller は `veil_poller_epoll` / `veil_poller_kqueue`。macOS は BSD と同じ kqueue poller を再利用）。**NetBSD の `struct kevent` は歴史的な BSD 定義（FreeBSD/OpenBSD/macOS の `filter: i16`/`flags: u16`）と異なり `filter`/`flags` とも `uint32_t`（`udata` も `intptr_t` ではなく `void *`）のため、`src/runtime/reactor/kqueue.rs::make_kevent` は `#[cfg(target_os = "netbsd")]` で `TryInto<u32>` 版を別途用意して吸収する（FreeBSD/OpenBSD/macOS 側は無変更）。**Windows は `veil_poller_wsapoll` の cfg 名のみ発行済み（実装は未着手）。FreeBSD は追加で `--features aio`（`veil_aio`）により `TcpStream::read`/`write` を POSIX AIO（`aio_read`/`aio_write` + `EVFILT_AIO` 完了通知）へ切替可能（既定オフ、`src/runtime/reactor/aio.rs`。既存 kqueue readiness 経路は無改変・ゼロコストで維持）。**ただし `aio` は使うべきではない（B-63）**: POSIX AIO は 1 I/O あたり 3 syscall（submit + `aio_error` + `aio_return`）を要して readiness 経路（1 発）より遅く、FreeBSD 14.3 aarch64 実測で小レスポンスのスループットが 4〜5 割落ちる（大きなレスポンスでは有意差なし＝利点が無い）うえ、HTTP/2 で小レスポンスを高並行に返すとサーバが完全停止する。`full-freebsd` の既定からは除外済み。**io_uring パスのロジックは変えない**（reactor 追加でも uring 生成コードを等価に保つ）。公開パス `runtime::tcp` 等はファサード re-export で不変に保つ。
- **プラットフォーム別セキュリティ／kTLS は `target_os` で分岐**（F-120/F-125/F-126/F-131） — Linux: seccomp（バックエンド別に許可 syscall 分割・最小権限）/Landlock/CBPF、kTLS（`veil_ktls` = `feature="ktls"` かつ linux/freebsd。Linux 経路は `src/ktls.rs`/`src/ktls_rustls.rs`、FreeBSD 経路は `src/ktls_freebsd.rs` に完全分離し Linux ロジックは無変更）。FreeBSD: capsicum（cap_rights_limit / cap_enter / jail）+ kTLS 対応（`TCP_TXTLS_ENABLE`/`TCP_RXTLS_ENABLE`、F-126）。**capability mode（`cap_enter`）下の静的配信は dirfd + `openat`/`fstatat` + `O_RESOLVE_BENEATH` 相対化で完全動作（F-123）**：`cap_enter` 前に File ルート dirfd を確保（`security.rs` `init_static_dirfds`）し、`OpenOptions` を単一チョークポイントとして読み取り専用 open を dirfd 相対 `openat` へ、`get_file_info`/`load_backend` の stat を `fstatat` へ切替。cap-mode では `std::thread::sleep`（内部 `clock_nanosleep(CLOCK_MONOTONIC)`）が `ECAPMODE` で panic するため、背景常駐スレッドは `select(2)` ベースの `server::cap_safe_sleep` を使う（非 FreeBSD は std sleep 委譲・挙動不変）。**capability mode 下の TLS 証明書ホットリロードも同じ dirfd 相対化で完全動作する（F-136）**：`cap_enter` 前に cert/key それぞれの親ディレクトリ fd を確保（`security.rs` `capsicum::init_tls_cert_dirfds`）し、`src/tls_reload.rs` の単一チョークポイント `read_pem`/`pem_mtime` が登録済みパスに対して dirfd 相対 `openat`/`fstatat`（`O_RESOLVE_BENEATH`）を使う（未登録パス・非 FreeBSD は `std::fs` のまま不変）。HTTP/3（quiche）は下記の in-memory `SSL_CTX` 経路と組み合わせて対応する。OpenBSD: pledge / unveil（kTLS 非対応、simple_tls フォールバック）。**NetBSD: pledge/unveil に相当するランタイム API が無い（F-140）** — 実装可能なのは `chroot(2)`+`chdir("/")`（`security.rs` の `netbsd` モジュール、設定キー `chroot_dir`、`drop_privileges` より前に適用）と特権降格（`setgroups`/`setgid`/`setuid`、既存の `#[cfg(unix)]` 共通実装がそのまま動作）・rlimit（`RLIMIT_NOFILE` 等、既存の共通実装がそのまま動作）のみで、非対応であることを起動時ログで正直に報告する（`netbsd::report_security_support`）。kTLS 非対応・simple_tls フォールバック。**OpenBSD の WASM は wasmtime の Pulley インタープリタで実行する（B-52）**: OpenBSD 6.4+ は SP が `MAP_STACK` 領域を指すことをカーネルが強制するが、wasmtime の async ファイバスタックは `MAP_STACK` 無しで mmap されるため wasm 実行の瞬間に SIGSEGV する。`src/wasm/registry.rs` で OpenBSD のみ (1) `InstanceAllocationStrategy::OnDemand`（`Config::with_host_stack` は **OnDemand でしか参照されず、プーリングでは黙って無視される**）(2) `MAP_STACK` 付きファイバスタック（`src/wasm/openbsd_stack.rs`）(3) `Config::target("pulley64")`（ネイティブコードを生成しないため配布時に `wxallowed` マウントを要求しない。速度はインタープリタ相当）を指定する。他ターゲットは無変更。**NetBSD の WASM も常時 Pulley（F-140）**だが、`MAP_STACK` の強制は OpenBSD 6.4+ カーネル固有の制約で NetBSD には無いため、`openbsd_stack`（MAP_STACK 付きファイバスタック）・OnDemand アロケータ強制は NetBSD には追加しない（通常の Pooling アロケータのまま）。**ただし NetBSD は代わりに PaX MPROTECT がシステム全体で強制されている（`security.pax.mprotect.enabled`/`.global` = 1、B-60）**: これは wasmtime が Pulley インタープリタ実行でも必要とする実行可能メモリの mmap/mprotect を EACCES で拒否するため、`on_request_headers`/`on_response_headers` 等のホスト関数呼び出しが軒並み失敗する（コード側の回避策は無く、`paxctl(8)` でバイナリの MPROTECT 制限を明示的に解除する運用対応のみ）。`paxctl +m` の適用は `tests/e2e_setup.sh`（NetBSD 実行時に自動）・`tools/qemu/bsd-vm.sh`（`cmd_build` で NetBSD ゲストに自動適用）・`packaging/scripts/build-bsd.sh`（NetBSD ホストで実行時はパッケージ前に適用、それ以外はインストール手順に明記）の 3 箇所に組み込み済み。**NetBSD（全アーキ）・FreeBSD aarch64・OpenBSD aarch64 も Proxy-Wasm が使える（B-55 解消）**: wasmtime 40 のシグナルベーストラップ実装（`signals.rs`）にはこの 3 ターゲット向けの `ucontext` 分岐が一切無く、crates.io 版をそのまま使うと Pulley の指定より前の wasmtime 自身のビルドで `compile_error!("unsupported platform")` になる（NetBSD は x86_64 ですら非対応）。これを解消するため crates.io wasmtime 40.0.4 を `third_party/wasmtime`（パッケージ名のみ `veil-wasmtime` に変更、詳細は `third_party/wasmtime/README.veil.md`）として vendoring し、`build.rs` に 2 行の差分（対象 3 ターゲットで `has_native_signals = false` を強制）を加えたものを Cargo の**ターゲット別依存**（`[target.'cfg(...)'.dependencies]`）で選択させる。`[lib] name = "wasmtime"` は据え置きのため `src/` の import は無変更。対象 3 ターゲットは常に Pulley インタープリタで実行（`src/wasm/registry.rs` の `veil_wasm_nosignals` cfg、既存の OpenBSD 強制 Pulley と統合）。それ以外の全プラットフォーム（Linux/Windows/macOS/FreeBSD x86_64/OpenBSD x86_64）は crates.io の wasmtime 40.0.0 をソース・依存とも一切変えずそのまま使う。`full-freebsd`/`full-openbsd`/`full-netbsd` はいずれも `wasm` を含む。**feature セットはアーキテクチャ非依存**（旧 `full-*bsd-aarch64` は基底セットと完全同一だったため廃止。vendoring 版 wasmtime の選択は feature ではなくターゲット別依存で行うため arch ごとに分ける必要が無い）。**macOS**: `sandbox_init`（Seatbelt、`src/security.rs` の `macos_sandbox` モジュール。実機検証不可のため保守的な deny-default + 書き込みのみ制限プロファイル、kTLS 非対応・simple_tls フォールバック）。**rustls 暗号プロバイダも `target_os` で分岐**（F-122/F-131/F-140、`src/tls_provider.rs` と Cargo.toml の target 別依存を一致させる）: **非 OpenBSD/NetBSD（Linux/FreeBSD/macOS/Windows） = `aws_lc_rs`**（cmake/nasm を含むクロスビルド環境で動作、無条件依存）、**OpenBSD/NetBSD = `ring`**（無条件依存。NetBSD は未検証だが OpenBSD と同じ Tier 3 サポート不足の懸念があるため保守的に合わせた）。**`http3`(quiche) の暗号バックエンドも `target_os` で分岐する**（F-136）: **Linux のみ** `default-features = false` で **rustls と同じ `aws-lc-sys` を共有**（非プレフィックス、memfd 経由のパス指定証明書ロードを無変更維持、無条件依存）。**FreeBSD/macOS/Windows/OpenBSD/NetBSD は quiche `boringssl-boring-crate` feature を無条件依存として使う**（外部 `boring` crate、`Cargo.toml` に `boring = "4.3"`（quiche が依存するバージョンと一致必須）を明示依存として追加。**Linux に `boring` を依存させてはならない**＝重複シンボルでリンクが壊れる）。FreeBSD は元々 Linux と `aws-lc-sys` を共有していたが、F-136（capsicum capability mode 下での証明書ホットリロード）のために `boringssl-boring-crate` 側へ移した: `quiche::Config::with_boring_ssl_ctx_builder`（PEM バイト列から直接 `boring::ssl::SslContextBuilder` を組む in-memory API、パス・ファイル・memfd 不要）がこの feature でしか提供されないため。`aws-lc-sys`（`NO_PREFIX=1`）と外部 `boring` crate を同一バイナリに同居させるとリンク時に重複シンボルエラーになることを実験で確認済み（`docs/artifacts/f136_platform_design.md`）。`src/http3_server.rs` の quiche `Config` 構築は `#[cfg(target_os = "linux")]`（従来の memfd 経路、無変更）と `#[cfg(not(target_os = "linux"))]`（in-memory `SSL_CTX`、初回ロード・リロード共通）に分岐する。この切替は `AWS_LC_SYS_NO_PREFIX` であり、**設定箇所は [.cargo/config.toml](.cargo/config.toml) の `[env]` 1 箇所のみ**（Linux=`1`、FreeBSD/Windows/macOS/OpenBSD/NetBSD=`0`）。cargo にターゲット別 env は無く `[target.<triple>.env]` は**黙って無視される**ため、aws-lc-sys が優先して読むターゲット接尾辞付き変数名 `AWS_LC_SYS_NO_PREFIX_<triple_with_underscores>` を列挙する。`build.rs` から `set_var` しても依存クレートの build.rs は先に別プロセスで動くため効かない。Dockerfile / packaging スクリプトでこの変数を設定してはならない（B-47）。非対象 OS 用の設定キーは受理し警告して無視する。README の前提と矛盾させない。**SSL ライブラリの動的リンク対応（system-tls、F-137/F-142）は 2026-07-30 に撤回した**（後方互換性は考慮不要と判断し、単純化のため巻き戻した）。撤回の経緯・実機で判明した知見（quiche が quictls 専用の QUIC API を要求すること、`rustls-openssl` が LibreSSL 非対応であること等）は `docs/backlog/features/F-137-system-tls-feature.md`（Withdrawn）・`docs/backlog/features/F-142-libressl-crypto-provider.md`（Withdrawn）参照。**HTTP/3 ライブラリの使い分け方針**: `quiche` は**本番データプレーン専用**（`src/http3_server.rs` のみ）とし、**テスト・計測ツールは `quinn` + `h3` を使う**（`tests/common/http3_client.rs`、`tools/perf/h3load`）。両者を同一プロセス内で混在させると上記のターゲット別 TLS バックエンド切替が複雑化するため、役割で明確に分離している。
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

## ビルドプロファイル（F-147）

- **`[profile.release]` は cargo 既定のまま**（`lto=false` / `codegen-units=16`）。
  Linux x86_64・FreeBSD aarch64 の両方で実測した結果、**LTO はスループットに影響せず
  効くのはバイナリサイズだけ**だったため、日常のビルド（開発・E2E・perf 反復）を
  遅くしてまで有効にしない（fat LTO は FreeBSD 増分ビルドで実測 26 秒 → 4 分 25 秒）。
- **配布バイナリは `[profile.dist]`**（`inherits = "release"` + `lto="fat"` +
  `codegen-units=1` + `strip="symbols"`）。実測 36.8MB → 25.3MB（**-31%**）。
  packaging のみが使う: `docker/Dockerfile.*`（builder ステージ）、
  `packaging/scripts/build.sh`、`tools/qemu/bsd-vm.sh`（`CARGO_PROFILE=dist`）。
  `--release` はシンボルを残すので DTrace とバックトレースが読める（perf 調査に必須）。
- **`panic = "abort"` は使用禁止**。`src/system.rs` の `CatchUnwindFuture` が
  `catch_unwind` でコネクション単位の panic を捕捉してワーカーを生かす設計であり、
  abort にすると 1 本の不正リクエストでプロキシ全体が落ち、`ConnectionGuard` の
  Drop（接続数カウンタ）も走らない。

## ホットパスの落とし穴（実測で判明）

- **`config::load_backend` はリクエストごとに呼ばれる**（`upstream.rs` の
  `find_backend_unified` がルート照合のたびに Backend を組み立てる）。ここに
  同期 FS 呼び出しを足すと即座に「1 リクエスト 1 syscall」になる（B-64 で
  `canonicalize`/`fs::metadata` を実測検出。設定に対して不変な値は
  スレッドローカルにメモ化すること）。**`#[allow(clippy::disallowed_methods)]` の
  「コールドパスだから安全」という根拠を鵜呑みにしないこと**（B-64 のものは事実と
  異なっていた）。
- **性能改善は必ず交互 A/B で確認する**。計測環境（QEMU VM）は同一バイナリでも
  ラウンド間で 1.8 倍変動する。時間をまたいだ比較は無意味
  （B-64 では syscall を 2 つ消しても中央値に差が出なかった＝そこはボトルネックでは
  なかった、という結論を交互 A/B で初めて確定できた）。

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
- `default = ["ktls", "http2", "mimalloc"]` を崩して重い依存をデフォルト必須にすること。（BSD 向けのアロケータ差し替えは `full-freebsd` / `full-openbsd` という**別 feature セット**で表現し、packaging のスクリプトが `--no-default-features` と併せて明示指定する。default を target 別に変えようとしないこと）
- 検証なしの `unsafe` 拡大、安易な `#[ignore]`（やむを得ない場合は理由を文書化）。
- `docs/artifacts/` 以外への AI 専用成果物の散乱。
- バックログの個別 md だけ更新して [docs/backlog/backlog.md](docs/backlog/backlog.md) を更新しないこと。

---

## ビルド・テスト（入り口）

詳細・feature 組み合わせ・E2E・ベンチは **README の Build / Testing 節**を参照。

### 注意事項
- **コンパイル時の依存関係**: `entry.rs`/各モジュールは `http2` や `grpc`（もしくは `grpc-full`）feature が有効でないと、`send_grpc_trailers` の呼び出し箇所等でコンパイルエラーが発生します。そのため、ビルドやテストの際は必ず十分な feature（例：`--features "http2,grpc-full"` またはフルフィーチャー）を指定して実行してください。
- **reactor バックエンドの E2E を必ず回すこと（F-145 の教訓）**: Linux の既定ビルドは io_uring バックエンド（`veil_rt_uring`）で、`src/runtime/reactor/` を **1 行もコンパイルしません**。reactor は BSD 全種・macOS・コンテナ向け `full-container` が使う本番経路であるため、`src/runtime/reactor/` に触れた場合は `VEIL_E2E_FEATURES="full,epoll" ./tests/e2e_setup.sh test` を必ず実行してください。F-145 では「最初の 1 リクエストでサーバがハングする」重篤な不具合が、単体 816 件・統合 53 件・E2E 541 件のすべてを通過しています（既定ビルドが当該コードを含まないため）。
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
| `third_party/wasmtime/` | crates.io wasmtime 40.0.4 の vendoring（B-55、パッケージ名のみ `veil-wasmtime`）。NetBSD 全アーキ・FreeBSD/OpenBSD aarch64 向けに `build.rs` を 2 行だけ差し替え、Pulley 専用でビルド可能にする。詳細・追従手順は `third_party/wasmtime/README.veil.md` |
| `docker/` | コンテナイメージ（glibc/musl、aarch64 版）・**非 Linux 向けクロスビルド用 `Dockerfile.{macos,windows,freebsd}`**（`Dockerfile.glibc` と同じ cacher/builder 2 段構成でキャッシュを効かせ、最終 `artifact` ステージを `--output type=local` で取り出す。B-47）・共有アセット（`assets/`：ssl/www/seccomp/Landlock） |
| `tools/` | Docker ベースの外形検証ツール。`tools/perf/` は glibc/musl/nginx 比較のパフォーマンス計測ハーネス（`gen_configs.sh` で 2⁴=16 直交表 + full features 機能ショーケース `feat_*` 構成 + **全プロトコル×全機能マトリクス（F-114: `h2_1_proxy_*`/`h3_file_*`/`h3_proxy*`/`grpc_h2_*`/`grpc_h3*`）** を生成 /`run_perf.sh` で反復計測（`CONFIG_GLOB` で scoped 化・gRPC over H3 はクライアント非対応で NA フェイルセーフ・TSV 行順は nginx → veil_glibc → veil_musl を保証、L4 構成は平文 9080 の readiness も確認 = F-118） /`analyze_results.sh` で median±stdev 集計。HTTP/1.1=wrk・HTTP/2=h2load・HTTP/3=QUIC 対応 h2load(`h2load-http3/`)・gRPC/WebSocket=grafana k6(`k6/`) の各クライアントで計測）、`tools/container_security/` はファジング・カオス・h2spec・セキュリティスキャンのオーケストレータ（`run.sh`）。**`tools/qemu/` は他アーキ/他 OS の実カーネル上でのビルド・E2E・perf 検証**（Docker ヘルパ経由の full-system QEMU。helper は aarch64 と x86_64 の両ゲストを収録し、x86_64 ゲストは `/dev/kvm` があれば KVM 加速。**Docker が無いホストでは `VEIL_QEMU_NATIVE=1`（docker コマンド不在なら自動）でホストの qemu-system-* を直接起動する native モードになり、Apple Silicon macOS では aarch64 ゲストが HVF で加速される**）: **`bsd-vm.sh <os> <arch>` が FreeBSD/OpenBSD × x86_64/aarch64 の 4 通り**を統一インタフェースで扱う（setup/up/grow/provision/toolchain/build/e2e/fetch。`e2e` は VM 内で `tests/e2e_setup.sh test`、`fetch` は `packaging/build/` へバイナリ取得。OpenBSD は miniroot からの autoinstall = `openbsd-autoinstall.py`、NetBSD は x86_64/aarch64 とも配布の起動イメージ + シリアルログインでの鍵注入 = `netbsd-provision.py`）。Linux aarch64 は Docker クロスビルド成果物を VM へ持ち込む（`aarch64-vm.sh`+`run-e2e-aarch64.sh` スモーク / `linux-aarch64-e2e.sh` フル E2E。KVM 不可ホストでは TCG が実用不能）。従来の `fbsd-arm64-vm.sh` は smoke 専用の従来経路として残置。詳細は `tools/qemu/README.md`。計測結果は `docs/perf/`（サマリ + 計測履歴は `docs/perf/README.md`、生データは `docs/perf/results_raw.tsv` = tools/perf 出力のコピー。詳細な生ログは
`docs/artifacts/perf_reports/` に git 管理外で保持）。F-121 の HPACK Huffman デコード LUT はコミット済みの `src/http2/hpack/huffman_decode_table.rs` をビルドが直接使用する。再生成用スクリプトは git 非管理の `docs/artifacts/gen_huffman_decode_table.py`（正本は `HUFFMAN_ENCODE_TABLE`）。通常のビルド・テスト・CI に Python 生成は不要 |

細かいモジュール対応は `src/lib.rs` の `mod` と README の構成を参照。
