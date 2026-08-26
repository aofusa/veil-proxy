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
- **HTTP/3 UDP データプレーンは io_uring パイプライン化済み（F-130）** — 受信は `runtime::uring::udp_recv::PipelinedUdpRecv`（`[http3].mmsg_batch_size` 本の `IORING_OP_RECVMSG` を常時 in-flight に保つソフトウェアパイプライン。1 回の `recv_batch()` 完了で複数データグラムをまとめて拾い、消費後は `rearm_ready()` で 1 回の submit にまとめて再投入）、送信は `runtime::uring::udp_send::UringUdpSend`（`IORING_OP_SENDMSG` を GSO `UDP_SEGMENT` cmsg 付きで複数 SQE 同時 submit）。ホットパスに libc `recvmmsg`/`sendmmsg` は登場しない。**真の `IORING_RECV_MULTISHOT` + provided buffer ring（C2）は実装済みだが既定オプトイン**（`VEIL_H3_BUFRING=1` のときだけ試み、登録・アーム失敗時は自動的に C1 へフォールバックする。`udp_recv.rs` の `MultishotUdpRecv` と C1/C2 共通インタフェース `UdpRecvBackend`、`ring.rs` の `register_buf_ring`）。**既定にしていない理由は「実機で一度も動かせていない」から**である: 検証機の Linux 6.8.0-137-generic は `IORING_REGISTER_PBUF_RING` を**クリーンなリングでも liburing 経由でも `-EINVAL` で拒否する**（同じリングで `IORING_REGISTER_PROBE` は成功するので veil 側のラッパーの不具合ではない）。したがって multishot 受信のハッピーパスは純関数の単体テストでしか検証できておらず、交互 A/B も取れていない。**PBUF_RING が通るカーネルでの E2E と交互 A/B が既定化の条件**。なお buffer ring の登録は SQE オペコードではなく register 操作なので、`PROXY_ALLOWED_OPCODES` は増やさず `apply_restrictions` の register op 許可だけを追加している（新規オペコードでセキュリティサーフェスを広げない方針を維持）。`recvmmsg`/`sendmmsg` は DNS 解決と `VEIL_H3_MULTISHOT=0`/reactor ビルド時のフォールバック経路にのみ残る。
- **HTTP/3 メインループはイベント駆動（F-151）** — 接続マップ全体をスイープしてはならない。`init_h3` / `handle_writable_streams` / `process_h3_events` / `drive_proxy_streams` / 送出（`send_pending_packets`）はすべて **ダーティ接続集合**（`VecDeque<ConnKey>` + `Http3Handler::dirty`、`ConnKey = Rc<ConnectionId>`）に載った接続だけを対象にする。ダーティ化の契機は「データグラムを受信した」「タイマー期限が到来した」「バックエンドタスクが起床キューへ自 cid を積んだ（`ConnWaker`）」「前回パスで仕事が残っていた」の 4 つで、これで quiche/h3 が新イベントを生成しうる契機を網羅している。タイムアウトは **最小ヒープ + 遅延削除**（`Http3Handler::timer_deadline` と一致するエントリだけ有効）で、期限が来ていない接続に `on_timeout` を呼んではならない。**B-12 再発防止の不変条件**: `handle_writable_streams`/`process_h3_events`/`drive_proxy_streams` のいずれかが 1 件でも仕事をしたら、その接続はダーティのまま再投入する（`Done` を見る＝完全に静止したパスを 1 回通るまで降ろさない）。またダーティ接続が残っている間は `select` の sleep を **0** にする（残したままスリープすると 1 チャンクごとに最大 100ms 止まる）。接続 ID を持ち回るキューは `Rc` にし、ホットパスで `ConnectionId` をディープコピー（malloc）しないこと。タイマー再登録は**データグラムごとではなく**ダーティ処理ループで接続ごとに 1 回だけ行う。
- **HTTP/3 の「1 イテレーションで扱うデータグラム数」は最重要チューニング項目（F-151/F-152）** — この量だけを変えるとスループットが 3.2 倍動く。決めるものはバックエンドで異なり、**io_uring は `[http3] mmsg_batch_size`**（`IORING_OP_RECVMSG` のパイプライン本数）、**reactor は `[http3] recv_drain_max`**（drain ループ上限。F-152 で定数から設定可能化。既定 64 = 従来の定数と同値、クランプ 1..=4096）。**どちらも新しい定数でハードコードし直さないこと。** io_uring 経路は `recv_drain_max` を参照せず、受理して無視する。
- **静的配信のパス解決に `canonicalize()` を使わない（F-153）** — 解決は「静的ルートの dirfd 相対 open + カーネルの封じ込め」で行う（Linux は `openat2(2)` + `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`、FreeBSD は `openat` + `O_RESOLVE_BENEATH`。`src/cache/resolve.rs` の `open_beneath`）。**`canonicalize()` のコストは libc 実装依存で桁が違う**: glibc は `realpath` をユーザ空間で実装し**パスの全コンポーネントに `readlink`** する（実測 7 回/リクエスト・全件エラー）が、FreeBSD は `__realpathat` の単一 syscall で済む。**「同じ Rust の API だから同じコスト」と考えず、プラットフォームごとに syscall を数えること。** 封じ込めをカーネルに任せると検査と open が原子的になり **TOCTOU の窓も消える**（セキュリティ面でも優る）。**注意 1**: `RESOLVE_BENEATH` は**絶対シンボリックリンクをリンク先がルート内でも拒否する**（`EXDEV`）ため、`current -> releases/vNNN` のようなデプロイ形を壊さないよう `EXDEV`/`ELOOP` のときだけ**そのリクエストに限り** `canonicalize` + 含有チェックへフォールバックする（恒久メモ化は `ENOSYS`/`EPERM`/`EOPNOTSUPP` のときだけ）。**注意 2**: `openat2` を seccomp 許可リストへ入れ忘れると静的配信が全部 404 になる（B-13 と同じ失敗モード）。
- **rustls の暗号文はコピーせずに `writev(2)` へ渡す（F-150）** — rustls 書き込み経路で `conn.write_tls(&mut Vec::new())` を書いてはならない（暗号文の全 memcpy + malloc が 1 送信ごとに発生する）。`src/tls_writev.rs` の `flush_tls_writev` を使う。ヘッダとボディは `conn.writer()` へ両方積んでから **1 回だけ**フラッシュする（`write(2)` 2 回ではなく `writev(2)` 1 回）。
- **FreeBSD の software kTLS は既定で使わない（F-155）** — FreeBSD の kTLS は TLS レコード（16KB）ごとに暗号処理をカーネルワーカースレッドへディスパッチするため、コンテキストスイッチが秒間 40 万回に達して帯域が直列化する。**HW オフロード非搭載なら `ktls_enabled = false` が推奨**であり、計測ハーネス（`tools/perf/freebsd/run_perf_freebsd.sh`）の既定もこれに揃えてある。**比較対象の nginx が kTLS を使っていない構成で veil だけ kTLS を有効にして測ってはならない**（2026-08-16 までの「54KB で対 nginx 0.5」はこの不公平な設定が原因で、無効化するだけで対 nginx 1.09〜1.16 に逆転した）。
- **FreeBSD の静的配信はヘッダも `sendfile(2)` の `sf_hdtr` で 1 syscall にまとめる（F-155）** — `runtime::sendfile::sendfile_all_with_header` を使う（`write_all(header)` → `sendfile_all(body)` の 2 syscall に戻さないこと）。部分送信の状態は `header_sent`/`file_sent` の 2 変数のみで持ち、`sbytes`（ヘッダ+本体の合計送信量）の配分は純関数 `distribute_sbytes` に閉じ込める。**`nbytes == 0` は「ファイル終端まで送る」の意味になる**ため、`Content-Length` を超えて送らないようガードが必須。適用範囲は `ServerTls::is_plain()`（TLS 終端なし）の HTTP/1.1 静的配信のみで、**rustls ユーザー空間 TLS 経路に通すとファイル本体が暗号化されずに流れる（平文漏洩）**。HTTP/2・h2c は DATA フレーミングが要るのでそもそも `sendfile` に載らない。
- **veil の平文リスナー（`h2c_listen`）は h2c 専用で HTTP/1.1 を受け付けない（F-155 で明文化）** — 平文 HTTP/1.1 は**メインリスナー（TLS ポート）のプロトコル検出**（`detect_protocol_with_buffer` → `accept_plain`）経由でしか到達しない。平文 HTTP/1.1 を計測・テストするときは `http://` を **TLS ポート**へ投げること（h2c ポートへ投げると「Plain HTTP/1.1 not supported on H2C-only server」で切断され、0 rps になる）。
- **kqueue の readiness ヒントは read/write 両方向に持つ（F-141/F-155）** — `EVFILT_READ`/`EVFILT_WRITE` の `data` を `FdRecord::read_hint`/`write_hint` に保存し、`Readable`/`Writable` が確認用の `poll(2)` を省略する。**ヒントが無いときの `poll(2)` フォールバックを消してはならない**（消すと必ず kqueue 往復 1 回分のレイテンシが乗る）。consume-once（`take`）にして古いヒントが後続へ漏れないようにすること。
- **accept は 1 周回 1 接続にしない（F-155）** — reactor バックエンドでは nginx の `multi_accept` 相当の `TcpListener::accept_batch`（上限 32 件）でバックログを引き上げる。上限で必ず抜けてイベントループへ戻る協調的設計を崩さないこと。`Accept::poll` と `accept_batch` は `raw_accept_one` を共用し、accept4/macOS フォールバック・fd リーク防止の順序を二重管理しない。**io_uring 側には実装せず `src/entry.rs` の cfg 分岐で切り分ける**（io_uring パスのロジックは変えない方針）。
- **`reactor/tcp/` にメソッドを足すときは `unix.rs` と `windows.rs` の両方に足す（B-69）** — `veil_rt_reactor` は poller ごとに**別のソースファイル**を選ぶ（`veil_poller_epoll`/`veil_poller_kqueue` → `reactor/tcp/unix.rs`、`veil_poller_wsapoll` → `reactor/tcp/windows.rs`）。**したがって `--features epoll` が通ることは kqueue / wsapoll が通ることを何ら保証しない。** F-155 が `accept_batch` を `unix.rs` にだけ実装して呼び出し側（`entry.rs`・`l4/server.rs`）を切り替えた結果、**Windows は F-155 以降ずっとコンパイル不能**だった（`packaging/output/` の Windows 成果物の日付が F-155 の前で止まっていた）。**この種の不具合は単体 903 件・統合 54 件・E2E 544 件のすべてを通過する**（Linux の既定ビルドでも `--features epoll` でも `windows.rs` は 1 行もコンパイルされないため）。F-145 が記録した「reactor はテストの空白地帯」の**さらに一段深い版**である。検出手段は `packaging/scripts/build-cross.sh --target windows|macos` のクロスビルドのみ（macOS は `unix.rs` を共用するため Windows だけが漏れる）。
- **リスナーは必ず `server::create_listener` 経由で作る（F-156）** — `SO_REUSEPORT`（FreeBSD は `SO_REUSEPORT_LB`）による全ワーカーへの分散と、FreeBSD capsicum のリスナー fd 権利制限（`limit_listener_rights`）がこの関数に集約されている。**L4 だけが `TcpListener::bind` を直接呼んでいたため、(1) 設定の `threads` に関わらず 1 コアでしか動かず、(2) capsicum の権利制限が適用されていなかった**（doc コメントは「全経路が通る」と書いてあったが事実と違った）。新しいリスナーを足すときも必ずこの関数を通すこと。
- **ワーカーを増やすときはワーカー間で共有すべき状態を必ず洗い出す（F-156）** — L4 のマルチワーカー化では`rr_state`（ラウンドロビン）・`conn_counters`（LeastConn）・`listener_counter`（`max_connections`）をリスナーにつき 1 個だけ作って `Arc` で配る。ワーカーごとに独立させると **`max_connections` の上限がワーカー数倍に緩む（設定違反）** ほか、ロードバランシングが壊れる。ヘルスチェッカーも同様にリスナーにつき 1 回だけ起動する（ワーカーごとだと上流へのヘルスチェックがワーカー数倍になる）。
- **接続受理のホットパスで `to_string()` しない（F-156）** — クライアント IP は `http_utils::IpStr`（`[u8; 46]` のスタックバッファ）を使う。`peer_addr.ip().to_string()` は接続ごとの malloc になる。
- **FreeBSD 計測 VM のノイズは「数ラウンド方向が揃う」程度では超えられない（F-156）** — `h2c_file_plain` で同じ 2 バイナリの交互 A/B を 2 回取ったところ、1 回目は片方が 4 ラウンド全勝、2 回目はもう片方が勝ち、8 サンプルの平均はほぼ同一（0.434 対 0.431）だった。**4 ラウンド一致を根拠に回帰と判断して実装を撤回しかけた**（実際には差が無かった）。加えて、**その計測系が対象の機構を動かしているかを先に確認すること**: h2load/wrk は keep-alive で接続を張りっぱなしにするため、`accept` 系の最適化は計測開始時の数十回しか実行されずスループットにほぼ寄与しない（= 測っていない）。
- **A/B で別バイナリを使うときは basename を `veil` のままにする（F-156）** — ハーネスの後始末は `pkill -x veil`（プロセス名の完全一致）なので、`veil.f156` のような名前にすると**1 つも kill されない**。しかもリスナーは `SO_REUSEPORT`(_LB) なので、取り残しが同じポートを掴んだまま生き残り、カーネルが新旧プロセスへ接続を分散して**別バイナリの混合を計測する**。`/root/ab/<変種>/veil` のようにディレクトリで分けること。ハーネス側にも「起動前に veil が残っていたら中止する」ガードを入れてある。
- **HTTP/2・HTTP/3 の静的配信は `static_file_cache` と `open_file_cache` を必ずセットで有効にする（F-157）** — `cache::get_static_file_with_content`（`src/cache/static_file.rs`）の offload ゼロ経路は「**メタデータキャッシュがヒットしたときに限り本体キャッシュを参照する**」構造なので、**本体キャッシュ（`[static_file_cache]`）だけ有効にしても素通りして毎リクエスト offload の open+read に落ちる**（実測で `openat` が 1.0/req のまま変わらず、スループットも改善しなかった）。HTTP/2・HTTP/3 は DATA フレーム / QUIC ストリームへの再フレーミングが要るため `sendfile(2)` に載せられず（nginx は h2c でも `sendfile` + `sf_hdtr` でカーネル内完結できる）、この 2 段キャッシュが sendfile の等価物になる。**計測でも片側だけをチューニングして比較してはならない**（nginx 側にも `open_file_cache` を入れる。F-155 の kTLS と同じ失敗）。
- **ホットパスのコピー削減は「コピー元がキャッシュに載っているか」を先に確認する（F-157）** — 54KB の DATA 本体を `write_buf` へ memcpy している箇所を `sendmsg` の N 本 iovec によるゼロコピー送出に置き換えたところ、**交互 A/B 4 ラウンドすべてで回帰した**（対 nginx 0.886 → 0.807）。事前見積もりは memcpy を DRAM 帯域（5 GB/s → 1 リクエスト 5.4µs）で計算していたが、**同じファイルを毎回配信するワークロードではコピー元が L2/L3 に residence し続け、memcpy は見積もりよりはるかに安い**。一方 `sendmsg` の per-iovec コストは実在する。「大きなコピーを消せば速くなる」は自明ではない。
- **HTTP/2 の per-stream タスクは reactor でだけ「インライン初回 poll」する（F-158）** — `h2_task_spawner` は `veil_rt_reactor` のときだけ `TaskPool::spawn_inline`（future をスラブへ格納し、**そのタスク自身の実 Waker** で 1 回だけその場で poll。`Poll::Ready` ならエグゼキュータの ready キューへ一度も積まない）を使い、**io_uring では従来どおり `spawn()` を使う**。`spawn_inline`/`spawn_body_and_poll` は `runtime::reactor::executor` にのみ存在し、**`runtime::uring::executor` は F-158 以前と 1 バイトも変わらない**。**同一の変更が kqueue で +27.3%（3B、10/10 ラウンド・分布完全分離）、io_uring で −4.6%（`h2c_file`、12 ラウンド中 11 でベースライン勝ち）と正反対になったため**である。理由は消せる往復コストの差で、reactor の待機は `Readable::poll` の**同期 `poll(2)`** を伴う（この `poll(2)` フォールバックは削除禁止＝上項参照）のに対し、io_uring の同じ待機は `IORING_OP_POLL_ADD` の SQE 1 本が他の SQE とまとめて submit されるため元から安く、インライン poll 側の固定費（スロット確保・Waker 構築・`EXEC_STATE` の追加借用）と SQE バッチングの乱れだけが残る。noop waker で初回 poll してはならない（Pending 時に恒久ハングする）。**教訓: ホットパス最適化は「片方のバックエンドで効いた」ことをもう一方へ一般化してはならず、必ず両方で交互 A/B を取ること。**
- **io_uring で I/O ごとにカーネルタイマーを張ってはならない（B-72）** — `runtime::uring::timer` は
  **スレッドローカルのデッドライン最小ヒープ**であり、`Sleep::poll` / `Sleep::drop` は
  ユーザ空間のヒープ操作だけで **SQE も syscall も出さない**。カーネルへの `IORING_OP_TIMEOUT` は
  `executor::wait_for_completions` が park 直前に **最近接の live デッドラインへ 1 本だけ**アームする
  （`ctx->timeout_list` の長さは常に 0 か 1）。**不変条件**: live な `D_min` が存在するとき、
  アーム中の TIMEOUT の期限は必ず `D_min` 以下（崩れると park が寝過ごす）。
  アーム中 TIMEOUT が参照する `KernelTimespec` は **drop されない専用スレッドローカル**に置く
  （`Option` に入れて再アーム時に drop すると、submit がエラーを返して SQE が未提出のまま
  SQ に残った場合にダングリングポインタになる）。満了処理 `fire_expired` は
  **`run_ready_tasks` の前**で回す（後ろに置くと、起こしたタスクを実行しないまま park する）。
  **旧実装は `Sleep::drop` で「キャンセル SQE + submit の syscall を節約する」ためキャンセルを
  投げず、`timeout(READ_TIMEOUT, read)` の勝ち筋のたびにタイマーを 30 秒カーネルに残していた。**
  数千 rps で `ctx->timeout_list` が 10 万オーダーに肥大し、別経路（in-flight POLL_ADD の drop）が
  出す `IORING_OP_ASYNC_CANCEL` のカーネル側フォールバック（`io_timeout_cancel` →
  `io_timeout_extract` → `io_cancel_req_match`）が**毎回そのリストを線形走査**して、
  逆プロキシ経路の **CPU の 40.6%** を食っていた（`perf record` 実測）。
  静的配信は `timeout()` を 1 度も通らずリストが空のため無傷で、これが
  「静的は nginx の 2.2 倍なのにプロキシは 1.2 倍」の正体だった。修正で **+30.8%**。
  **教訓 1: 「syscall を 1 回節約する」局所最適化が、別経路の計算量を O(1) から O(n) へ
  引き上げることがある。ホットパスの判断はユーザ空間の命令数だけでなくカーネル側の
  データ構造まで見て行うこと**（`perf` でカーネルシンボルを必ず確認する）。
  **教訓 2: 蓄積するコストは「定常状態」で測る。** 本バグのコストは負荷継続秒数に依存して
  積み上がるため、起動直後に `-n 40000`（実測 4〜5 秒）を流すと蓄積前の有利な状態を測ってしまい、
  ベースラインを 13% 過大評価する（実測 8,765 対 7,757）。**A/B の各ラウンドで
  `READ_TIMEOUT` を超える持続負荷（40 秒）をかけてから計測すること**
  （`tools/perf/h2c_proxy_lab.sh ab` に組み込み済み）。
- **プロキシ応答の中間バッファをリクエストごとに確保しない（B-72 第 2 弾）** —
  `h2_relay_backend_response` はヘッダーが 1 回目の read で完結する通常ケースでは
  **プール済み受信バッファのスライスを直接** `parse_http_response` に渡し、`PooledBuf`
  （Drop で必ず `buf_put` する RAII）で持ち回る。中間 `Vec::with_capacity(BUF_SIZE)`（64KB）への
  確保 + `extend_from_slice` を復活させてはならない（+2.74%）。**ヘッダー解析回数を増やさないこと**:
  速い経路は `parse_http_response` 1 回 + `httparse::Response` 1 回の計 2 回で、
  1 回目の解析結果を捨てて後段で解析し直すと解析が 3 回になり、コピー削減分を打ち消す
  （実装中に実際に踏み、レビューで差し戻した）。**`queue_data_frames` の DATA フレーミング
  コピーには手を付けないこと**（F-157 が iovec 化の退行を実測済み）。
- **WASM の gRPC 呼び出しは専用スレッドで駆動する（F-139）** — `proxy_grpc_call`/`proxy_grpc_stream`/
  `proxy_grpc_send` の実行は `src/server.rs::spawn_wasm_grpc_thread`（条件変数 + `poll(2)` 駆動の
  背景専用スレッド）が担う。**WASM tick スレッドへ戻してはならない**: tick は既定 100ms の固定周期で、
  状態機械を回すと 1 ステップ 100ms になり、F-134 当時の「1 呼び出しブロッキング完結」よりレイテンシが
  悪化する。接続は `(host, port, tls)` ごとの HTTP/2 プール（`grpc_pool.rs`、1 接続 1 アクティブ
  ストリーム）で再利用し、`proxy_grpc_send` は half-close を待たず都度送出、応答メッセージは到着ごとに
  `proxy_on_grpc_receive` へ配送する。**F-106 の教訓（接続を再利用すると送信ウィンドウ枯渇が顕在化する）を
  踏襲し、接続ウィンドウとストリームウィンドウの両方を追跡すること。** ゲスト由来のメタデータは
  `GrpcMetadataBlob`（`Bytes`）のまま借用イテレータで HPACK へ渡し、ホスト関数（＝データプレーンの
  ワーカースレッド上で走るホットパス）でペアごとの `String` を確保しない（F-160）。
- **HTTP/3 の起床通知はコアレッシングする（F-161）** — `ConnWaker::notify()` はレスポンスの
  チャンクごとに呼ばれるため、接続ごとに共有する `queued: Rc<Cell<bool>>` でメインループが drain するまでの
  重複 push を省く。drain は **pop → `flag.set(false)` → `mark_dirty`** の順で行う（flag を後に戻すと
  `mark_dirty` 中に発生した通知を取りこぼす）。
- **HTTP/2 の per-stream 固定費は `h2_spawn_for_request` に集まる（F-162）** — 1 ストリーム 1 リクエストなので、
  ここに置いた処理はそのままリクエスト単価になる。`set_host` は `&str` 受け（`metrics` 無効時に確保しない）、
  クライアント IP は接続あたり 1 個の `Rc<str>`、クライアント `SocketAddr` は接続あたり 1 回の解決。
  **新しい per-stream 処理を足すときは「接続あたり 1 回で済まないか」を先に考えること。**
- **epoll reactor は fd あたり `epoll_ctl` を生涯 1 回しか呼ばない（F-166 A-2）** —
  `EPOLLONESHOT` + 待機ごとの `EPOLL_CTL_MOD` は廃止し、初回登録時に
  `EPOLLIN|EPOLLOUT|EPOLLRDHUP|EPOLLET` で **1 回だけ ADD** する。以降の
  `executor::register` は Waker を積むだけで **syscall を出さない**。
  **ET のエッジ取りこぼしを塞ぐ不変条件は 2 つ**: (1) `dispatch_event` が立てる
  `read_hint`/`write_hint` は **consume-once**（`take`）で、消費した側は必ず直後に
  非ブロッキング I/O を試す。(2) ヒントが無い状態で park する前に **必ず確認用
  `poll(2)` を通す**（この `poll(2)` フォールバックは削除禁止＝kqueue 版と同じ）。
  fd は close まで epoll の監視対象に残るため、register 前に届いたエッジも
  次の `epoll_wait` で配送される。**実測: `h2c_proxy` の 1 リクエストあたり syscall が
  54KB で 5.97 → 4.72（-21%）、3B で 5.47 → 3.83（-30%）、`epoll_ctl` は 1.25〜1.30 → 0.02〜0.13。**
- **fd ごとの待機者は `WakerSlot`（Empty/One/Many）で持つ（F-166 A-3）** — 1 fd 1 待機者が
  支配的なので、`Vec<Waker>` を常用すると待機・起床のたびに malloc/free が乗る。
  複数待機（`runtime::offload` の共有 eventfd）は `Many` で従来どおり全員起床させる
  （**先行者の Waker を上書き消失させない**という F-120 Phase 2 の不変条件は維持）。
- **HTTP/2 レスポンスヘッダは `Vec<(Bytes, Bytes)>` で持つ（F-165/F-166 B）** —
  `H2RespMsg::Head`/`Trailers` の型を `Vec<(Vec<u8>, Vec<u8>)>` に戻してはならない。
  定数ヘッダは `Bytes::from_static`、`server`/`alt-svc` の値は `pool.rs` が `Bytes` で
  保持して**参照カウント clone** で配る（レスポンスごとの確保ゼロ）。バックエンド応答の
  ヘッダは **ムーブ**する（`(k.clone(), v.clone())` のディープコピーを復活させない）。
- **アロケーションは推測せず `alloc-stats` で測る（F-165）** — `--features alloc-stats`
  （既定オフ・opt-in）でグローバルアロケータがカウンティング版に差し替わり、
  `veil_alloc_allocs_total` 等の Prometheus ゲージで 1 リクエストあたりの確保回数を
  実測できる。ハーネスは `tools/perf/alloc_measure.sh`。
  **実測の起点（2026-08-26）**: `h2c_file` 3B 静的で 33.4 allocs/req、
  `h2c_proxy` 54KB で 50.7 allocs/req・61KB/req。**静的配信でも 30 回超確保している＝
  HTTP/2 サーバ側の固定費が支配項**であり、削減対象を「プロキシ経路」と決め打ちしないこと。
- **`[server].tls_only` は既定 `true`（F-163）** — メインリスナー（`[server].listen`）は
  平文を受理しない。`true` のときはプロトコル検出（MSG_PEEK）自体を行わないので
  接続ごとの往復も消える。平文 h2c / HTTP/1.1 を使うテスト・計測は
  `h2c_listen` の専用ポート（または `tls_only = false`）を使うこと。
- **UDS リスナーは 1 回 bind して各ワーカーが `dup(2)` する（F-164）** — AF_UNIX に
  `SO_REUSEPORT` は無い。`server::bind_unix_listener` が唯一の bind 経路で、
  stale socket の unlink・`unix_socket_permissions`（umask + chmod）・capsicum の
  権利制限をここに集約する。peer アドレスは `sockaddr_un` を `SocketAddr` へ変換できないため
  **プレースホルダ `127.0.0.1:0`** を返す（IP ブロックリスト・アクセスログはこの値を見る）。
- **Windows のクロスビルドを壊していないか確認する（B-69 / B-73）** — `cfg(unix)` を
  付け忘れた `std::os::unix::*` / `libc::poll` は Linux の単体・統合・E2E をすべて通過する。
  検出手段は `packaging/scripts/build-cross.sh --target windows` のみ。
  **2026-08-26 時点で B-73（F-139 の `wasm/host/grpc_executor.rs`）により失敗する。**
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
- **ルートに紐づく派生値は `Route` に事前計算して持たせる**（F-148）。`load_backend` が
  リクエストごとに呼ばれる以上、そこで `Arc::new(x.clone())` するものは 1 リクエスト 1 アロケーション
  になる。WASM モジュールリストは設定ロード時に合成済みの `Route::resolved_modules:
  Option<Arc<Vec<ModuleRef>>>` を作り、`load_backend` は `Arc` を clone するだけにしてある。
  新しいルート単位設定を足すときも同じ形（`#[serde(skip)]` の解決済みフィールド + ロード時に埋める）にすること。
- **`load_backend` が作っているのはモジュールリストだけではない（F-159）**。F-148 で
  `resolved_modules` を事前解決したあとも、`load_backend` は**リクエストごとに**
  `security`/`compression`/`buffering`/`cache` を 2 回ディープコピーし（ローカルへ 1 回、
  `Arc::new` でもう 1 回）、単一 URL プロキシでは `ProxyTarget::parse` + `UpstreamGroup` を
  組み立て直し、File ルートでは `Arc<PathBuf>`/`Arc<str>` を確保し、**`mode = "memory"` に
  至ってはリクエストごとに `fs::read` していた**（さらに圧縮・キャッシュ有効時は
  `info!` をリクエストごとに出していた）。現在は `Backend` 本体・圧縮設定・パスプレフィックスを
  設定ロード時に解決して `Route` の `#[serde(skip)]` フィールドへ持たせ、ホットパスは
  `Arc` の clone だけにしてある（`resolved_backend`/`resolved_compression`/`resolved_path_prefix`)。
  **ルート単位の派生値を新しく足すときは、必ずこの 3 つと同じ「ロード時に解決 → ホットパスは clone」の
  形にすること。** 解決に失敗したルートは `None` のままにして従来の毎回構築経路へ落とし、
  エラー挙動の後方互換を保つ（`build_backend` が実体）。実測 h2c_file 3B +7.41% / h2c_proxy 54KB +2.65%。
- **削ったアロケーションの効果は「固定費が支配的なワークロード」でしか見えない（F-159/F-162）**。
  同じ変更が 3B（固定費支配）では +7.41%/+0.89%、54KB（バイト単価支配）では +2.65%/ノイズ内、と
  はっきり分かれる。**両方のサイズで測り、差の出方が理論と整合するかまで確認すること**
  （54KB だけ見て「効かない」と結論しない／3B だけ見て「大改善」と誇張しない）。
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
#
# 注意: `--bins` はバイナリターゲット（main.rs）のテストで **0 件** である。
# 単体テスト（903 件）は **ライブラリ側**にあるため `--lib` が必須。
# `--bins --test integration_tests` だけだと統合 54 件しか走らず、
# 単体テストを 1 件も実行しないまま「ok」になる（実際に踏んだ）。
cargo test --lib --bins --test integration_tests --features "full"
```

---

## ディレクトリ要約

| パス | 役割 |
|------|------|
| `src/main.rs` | 薄いバイナリエントリ（`veil::run()` を呼ぶだけ） |
| `src/lib.rs` | クレートルート・mod 宣言・公開 API（`cargo fuzz`・統合テスト向け） |
| `src/entry.rs` | サーバ起動配線（`run()`：ワーカースレッド・accept ループなど） |
| `src/runtime/` | 独自ランタイム。共有（buf.rs/io.rs/offload.rs）+ `uring/`（io_uring: ring/executor/tcp/timer/splice、`veil_rt_uring`。HTTP/3 UDP はパイプライン化 `IORING_OP_RECVMSG`（`udp_recv.rs` の `PipelinedUdpRecv`）/ `IORING_OP_SENDMSG`（`udp_send.rs` の `UringUdpSend`）= F-130）+ `reactor/`（epoll/kqueue readiness: poller/epoll/kqueue/executor/tcp/timer/splice、`veil_rt_reactor`）。バックエンドは build.rs 発行 cfg で選択、公開パスはファサードで不変（F-120） |
| `src/tls_writev.rs` | rustls 暗号文の `writev(2)` 直接送出（F-150）。`FdVectoredWriter`（`io::Write` 実装。`write_vectored` が `libc::writev` / Windows は `WSASend` を直接発行）と共通フラッシュヘルパ `flush_tls_writev`。rustls の `ConnectionCommon::write_tls` が内部の暗号文チャンク列を `write_vectored(&[IoSlice; <=64])` **1 回**で吐き出す（`ChunkVecBuffer::write_to`）性質を利用し、**中間 `Vec` への memcpy と malloc/free を完全に消す**。`simple_tls.rs` / `ktls_rustls.rs` の rustls 書き込みループはすべてこれを使う（kTLS オフロード経路は無変更） |
| `src/wasm_plugin_config.rs` | Proxy-Wasm プラグイン設定（F-148）。`configuration` の型 `PluginConfiguration`（untagged: 文字列 = 従来互換 / TOML テーブル = JSON へ変換）、ルート単位上書きの合成 `merge_over`、`serde_json` を使わない自前 TOML→JSON エンコーダ、ルートが持ち回る `ModuleRef`（名前 + 解決済み設定）。**`wasm` feature に依存せず常にコンパイル**（`config.rs` の `Route` が型として使うため）。合成・変換は設定ロード時のみのコールドパス |
| `tests/`、`benches/` | 統合・E2E・ベンチ（`cargo test` が拾うホワイトボックス） |
| `examples/config.toml` | 設定リファレンス（全キー網羅・`src/config.rs` 同期） |
| `docs/readme/` | 日本語 README（`README.ja.md`） |
| `docs/artifacts/` | AI 成果物・一時ファイル |
| `docs/backlog/` | 機能・バグチケット（親は `backlog.md`） |
| `third_party/wasmtime/` | crates.io wasmtime 40.0.4 の vendoring（B-55、パッケージ名のみ `veil-wasmtime`）。NetBSD 全アーキ・FreeBSD/OpenBSD aarch64 向けに `build.rs` を 2 行だけ差し替え、Pulley 専用でビルド可能にする。詳細・追従手順は `third_party/wasmtime/README.veil.md` |
| `docker/` | コンテナイメージ（glibc/musl、aarch64 版）・**非 Linux 向けクロスビルド用 `Dockerfile.{macos,windows}`**（`Dockerfile.glibc` と同じ cacher/builder 2 段構成でキャッシュを効かせ、最終 `artifact` ステージを `--output type=local` で取り出す。B-47）・共有アセット（`assets/`：ssl/www/seccomp/Landlock）。**FreeBSD 向け `Dockerfile.freebsd` は B-49（未解決）により削除済み** — FreeBSD は QEMU VM 内ネイティブビルド（`tools/qemu/bsd-vm.sh freebsd <arch> build`）が唯一の公式経路 |
| `tools/` | Docker ベースの外形検証ツール。`tools/perf/` は glibc/musl/nginx 比較のパフォーマンス計測ハーネス（`gen_configs.sh` で 2⁴=16 直交表 + full features 機能ショーケース `feat_*` 構成 + **全プロトコル×全機能マトリクス（F-114: `h2_1_proxy_*`/`h3_file_*`/`h3_proxy*`/`grpc_h2_*`/`grpc_h3*`）** を生成 /`run_perf.sh` で反復計測（`CONFIG_GLOB` で scoped 化・gRPC over H3 はクライアント非対応で NA フェイルセーフ・TSV 行順は nginx → veil_glibc → veil_musl を保証、L4 構成は平文 9080 の readiness も確認 = F-118） /`analyze_results.sh` で median±stdev 集計。HTTP/1.1=wrk・HTTP/2=h2load・HTTP/3=QUIC 対応 h2load(`h2load-http3/`)・gRPC/WebSocket=grafana k6(`k6/`) の各クライアントで計測）、`tools/container_security/` はファジング・カオス・h2spec・セキュリティスキャンのオーケストレータ（`run.sh`）。**`tools/qemu/` は他アーキ/他 OS の実カーネル上でのビルド・E2E・perf 検証**（Docker ヘルパ経由の full-system QEMU。helper は aarch64 と x86_64 の両ゲストを収録し、x86_64 ゲストは `/dev/kvm` があれば KVM 加速。**Docker が無いホストでは `VEIL_QEMU_NATIVE=1`（docker コマンド不在なら自動）でホストの qemu-system-* を直接起動する native モードになり、Apple Silicon macOS では aarch64 ゲストが HVF で加速される**）: **`bsd-vm.sh <os> <arch>` が FreeBSD/OpenBSD × x86_64/aarch64 の 4 通り**を統一インタフェースで扱う（setup/up/grow/provision/toolchain/build/e2e/fetch。`e2e` は VM 内で `tests/e2e_setup.sh test`、`fetch` は `packaging/build/` へバイナリ取得。OpenBSD は miniroot からの autoinstall = `openbsd-autoinstall.py`、NetBSD は x86_64/aarch64 とも配布の起動イメージ + シリアルログインでの鍵注入 = `netbsd-provision.py`）。Linux aarch64 は Docker クロスビルド成果物を VM へ持ち込む（`aarch64-vm.sh`+`run-e2e-aarch64.sh` スモーク / `linux-aarch64-e2e.sh` フル E2E。KVM 不可ホストでは TCG が実用不能）。従来の `fbsd-arm64-vm.sh` は smoke 専用の従来経路として残置。詳細は `tools/qemu/README.md`。計測結果は `docs/perf/`（サマリ + 計測履歴は `docs/perf/README.md`、生データは `docs/perf/results_raw.tsv` = tools/perf 出力のコピー。詳細な生ログは
`docs/artifacts/perf_reports/` に git 管理外で保持）。F-121 の HPACK Huffman デコード LUT はコミット済みの `src/http2/hpack/huffman_decode_table.rs` をビルドが直接使用する。再生成用スクリプトは git 非管理の `docs/artifacts/gen_huffman_decode_table.py`（正本は `HUFFMAN_ENCODE_TABLE`）。通常のビルド・テスト・CI に Python 生成は不要 |

細かいモジュール対応は `src/lib.rs` の `mod` と README の構成を参照。
