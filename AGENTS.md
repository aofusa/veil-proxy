# AGENTS.md — Veil (veil-proxy)

AI エージェントおよびコントリビュータ向けの **最小指針**。利用者向けの説明は [README.md](README.md) と
[docs/guide/](docs/guide/README.md)（日本語は [docs/guide/ja/](docs/guide/ja/README.md)）、フィーチャー定義は
[Cargo.toml](Cargo.toml) の `[features]`。各規則の背景（実測値・事故の経緯）は
[docs/history/](docs/history/README.md) と `docs/backlog/` の各チケットにある。

---

## プロジェクトの事実

- クレート名 `veil`（ディレクトリ名 `veil-proxy`）。リリースバイナリは `target/release/veil`。
- **ライブラリ + バイナリ構成**。mod 宣言・公開 API は [src/lib.rs](src/lib.rs)、サーバ起動配線は
  [src/entry.rs](src/entry.rs)（`veil::run()`）、[src/main.rs](src/main.rs) は `veil::run()` を呼ぶだけ。
- 設定は TOML（`serde`）。全キーのリファレンスは [examples/config.toml](examples/config.toml)（`src/config.rs` と同期）。

---

## 設計哲学

**Rust の安全性を土台に、Linux カーネル（io_uring、kTLS、seccomp、Landlock、ソケット/CBPF）とユーザー空間を
噛み合わせ、HTTP/1.1・2・3 のデータプレーンを tokio/monoio なしで高スループット化・ゼロコピー化する。
io_uring は `src/runtime/` の独自実装（libc + bytes のみ）で直接操作する。開発効率を度外視して最大限の性能を
目指し、かつ運用で効く動的設定・観測・拡張（Proxy-Wasm）まで載せる。**

変更・レビューでは個別機能だけでなく **ここに反しないか** を意識する。

---

## ホットパス絶対規則（最優先・例外なし）

データプレーン（接続受理〜リクエスト/レスポンス転送〜TLS/HTTP/2/HTTP/3/WASM〜バックエンド I/O。
1 リクエスト / 1 コネクションあたり実行される全コード）では次を守る。

- **同期処理（ブロッキング呼び出し）禁止。** I/O・待機・syscall は `src/runtime/` の非同期 API（`.await`）で行う。
  `std::net`・ブロッキング `read/write/connect`・同期 DNS・`block_on`・`std::thread::sleep`・同期ロック待ちを置かない。
  対応する io_uring オペコードが無いブロッキング処理は `src/runtime/offload.rs` の `offload()` へ退避する
  （**新規 io_uring オペコードを増やしてセキュリティサーフェスを広げない**）。WASM 等の CPU バウンド処理も協調的に yield する。
- **アロケーション禁止（性能上必要な場合を除く）。** リクエストごとの `Vec`/`String`/`HashMap`/`Box`、
  `to_vec()`/`to_string()`/ディープ `clone()`/`format!`/`collect()` を増やさない。
- **ゼロコピー徹底。** `Bytes`/`BytesMut`、`src/pool.rs` のスレッドローカルプール、`splice(2)`/`sendfile(2)` を使う。
- **リクエストごとのログを `info!` 以上で出さない**（既定レベルで毎リクエスト出力される。必要なら `debug!`）。
- **難易度や保守性を理由に妥協しない。** 満たせない設計なら設計自体をやり直す。
- 変更のたびに「これはホットパスか？」を自問する。

---

## 設計制約（現役の不変条件）

詳細な経緯は括弧内のチケットと [docs/history/AGENTS-archive-2026-10.md](docs/history/AGENTS-archive-2026-10.md)。

### ランタイム・プラットフォーム

- データプレーンは tokio / monoio に依存しない（テスト・クライアント用途は別）。
- `cfg(feature)` を壊さない。`default = ["ktls", "http2", "mimalloc"]` を崩さず、無効 feature でもコンパイル可能に保つ。
  BSD 向けは `full-freebsd` / `full-openbsd` / `full-netbsd`、Windows 向け（mimalloc）・macOS 向け（システムアロケータ）は
  `full-windows` / `full-macos` という別 feature セットで表現する（F-184）。
- バックエンドは build.rs 発行の cfg で選ぶ: Linux 既定 = io_uring（`veil_rt_uring`）、`--features epoll`・BSD・macOS・
  Windows = readiness reactor（`veil_rt_reactor`、poller は `veil_poller_{epoll,kqueue,wsapoll}`）。
  **io_uring パスのロジックは reactor 追加で変えない。** 公開パス `runtime::tcp` 等はファサードで不変。
- `reactor/tcp/` にメソッドを足すときは `unix.rs` と `windows.rs` の両方に足す（B-69）。
- `cfg(unix)` は macOS で通ることを意味しない（`SOCK_NONBLOCK` 等は無い。B-81）。新しい `socket`/`accept` 系は
  `reactor/tcp/unix.rs` の `create_nonblocking_socket` の macOS 分岐を参照する。
- 非 unix でも型として存在するフィールドを見る分岐にも `cfg(unix)` が要る（F-170）。
- プラットフォーム限定関数だけが使う定数にも同じ `cfg` を付ける（`allow(dead_code)` で黙らせない）。
- FreeBSD の `aio` feature は使わない（B-63）。FreeBSD で `jemalloc` feature（tikv-jemalloc）は使わない
  （libthr の malloc フックを上書きして libc の jemalloc を壊す。compile_error 化済み。B-89）。NetBSD の `struct kevent` は型が違う（`make_kevent` の netbsd 版）。
- セキュリティ機構は `target_os` で分岐: Linux = seccomp/Landlock/CBPF、FreeBSD = capsicum（cap-mode 下の静的配信・
  証明書リロードは dirfd 相対 `openat`、背景スレッドの sleep は `server::cap_safe_sleep`。F-123/F-136）、
  OpenBSD = pledge/unveil、NetBSD = chroot + 特権降格のみ（F-140）、macOS = Seatbelt。非対象 OS のキーは警告して無視。
- WASM: OpenBSD は Pulley + OnDemand + `MAP_STACK` スタック（B-52）。NetBSD 全アーキ・FreeBSD/OpenBSD aarch64 は
  vendoring 版 wasmtime（`third_party/wasmtime`）をターゲット別依存で選び常に Pulley（B-55）。NetBSD は `paxctl +m` が必要（B-60）。

### TLS・暗号

- rustls プロバイダ: OpenBSD/NetBSD = `ring`、それ以外 = `aws_lc_rs`（`src/tls_provider.rs` と Cargo.toml を一致させる）。
- quiche（http3）: **Linux のみ** `aws-lc-sys` 共有（memfd 経路）、他 OS は `boringssl-boring-crate` + `boring = "4.3"`。
  **Linux に `boring` を依存させない**（重複シンボル）。`AWS_LC_SYS_NO_PREFIX_<triple>` は
  [.cargo/config.toml](.cargo/config.toml) の `[env]` 1 箇所のみで設定する（Dockerfile 等で設定しない。B-47）。
- HTTP/3: `quiche` は本番データプレーン専用、テスト・計測ツールは `quinn` + `h3`。
- rustls の暗号文は `src/tls_writev.rs` の `flush_tls_writev` で `writev(2)` へ直接渡す（中間 `Vec` 禁止。F-150）。
- rustls の受信は **`read_tls` ごとに平文を排出する**（受信平文 16KB 上限超過でエラーになる。B-86）。
- `Read`/`Write` ラッパ型は `read_vectored`/`write_vectored` を必ず委譲する（F-170）。
- FreeBSD の software kTLS は既定で使わない（F-155）。

### HTTP/2・HTTP/3・静的配信・プロキシ

- HTTP/3 メインループはダーティ接続集合でイベント駆動（全接続スイープ禁止）。仕事をした接続はダーティのまま再投入し、
  ダーティが残る間は sleep 0（B-12 / F-151）。起床通知はコアレッシングする（F-161）。
- HTTP/3 の 1 イテレーションのデータグラム数は `[http3] mmsg_batch_size`（io_uring）/ `recv_drain_max`（reactor）で
  決まる最重要チューニング項目。定数でハードコードし直さない（F-151/F-152）。
- HTTP/3 のストリーミング経路で本文途中のエラーは fin ではなくストリームリセットにする（切り詰めを成功に見せない。B-86）。
- HTTP/3 のバッファ経路（静的配信・gRPC・Wasm・`buffering = full` 等）の要求は、メインループで await しない。
  ワーカーの `FuturesUnordered` に置いて作成直後に 1 回 poll し、応答は `ProxyStream` のチャネルで返す。
  別タスクとして spawn すると `yield_now` まで走らず応答が 1 イテレーション遅れる（3B で −12%。B-97）。
- 上流 HTTP/2（h2c・ALPN で h2 を選んだ HTTPS）は `http2::upstream_mux` の多重化接続だけを使う（接続ごとに
  1 アクター、接続・ストリーム両方のフロー制御。F-174/B-106）。HTTP/1.1 のクライアントは `http2 = "on"` の
  ときだけ使う（`"auto"` で使うと 3B −12%・54KB −13.5%。F-175）。
- 上流のホスト名解決は `runtime::dns::resolve`（offload + キャッシュ）。`ToSocketAddrs` は clippy で禁止
  （起動時・専用スレッド・offload 内だけ理由付き allow。B-107）。
- レートリミットはプロセス共有のアトミック表で `(ルート, IP)` ごとに数える（thread_local に戻さない。B-31）。
- HTTP/3 のサーバ接続 ID はクライアントの元 DCID から HMAC で決定的に導出する（乱数にすると Initial の再送・
  複数パケットの ClientHello が届くたびに別接続を `accept` する。B-94）。
- BSD は上限を超える `SO_RCVBUF`/`SO_SNDBUF` を `ENOBUFS` で拒否して既定値のまま残す（Linux は黙って切り詰める）。
  失敗を無視せず通る最大値まで下げる（B-95）。負荷ツール側のソケットも同じ（取りこぼしがサーバの PTO として計測に乗る）。
- 同じタスク内で複数の I/O Future を同時に待つとき `futures::join!` を使わない。片方の起床で全員を poll するため
  reactor では起床していない側が `read(2)`/`poll(2)` を空打ちする（`FuturesUnordered` を使う。F-173）。
- quiche は `third_party/quiche` の vendoring 版を `[patch.crates-io]` で使う。主な差分は「複数ストリームの STREAM フレームを
  1 パケットへ詰める」1 行（upstream は fuzzing 時のみ。外すと小応答が 1 リクエスト 1 パケットに戻る。F-172）。
  `crate-type` は `["lib"]` だけにする（cdylib を戻すと cargo-zigbuild の macOS クロスビルドがリンクで失敗する。F-184）。
- 静的配信のパス解決に `canonicalize()` を使わない（dirfd 相対 `openat2`/`O_RESOLVE_BENEATH`。`openat2` は seccomp 許可必須。F-153）。
  絶対シンボリックリンクは Linux=`EXDEV`・FreeBSD=`ENOTCAPABLE` で拒否されるので、その場合だけ従来経路で再判定する（B-89）。
- FreeBSD の静的配信はヘッダも `sendfile(2)` の `sf_hdtr` で 1 syscall（平文 HTTP/1.1 のみ。TLS 経路に通すと平文漏洩。F-155）。
- HTTP/2・HTTP/3 の静的配信は `static_file_cache` と `open_file_cache` をセットで有効にして初めて offload ゼロになる（F-157）。
- HTTP/2 のインライン初回 poll（`spawn_inline`）は reactor 専用。io_uring 側の executor は変えない（F-158）。
- io_uring で I/O ごとにカーネルタイマーを張らない。`runtime::uring::timer` はユーザ空間の最小ヒープで、
  カーネルの `IORING_OP_TIMEOUT` は park 直前に 1 本だけ（B-72）。キャンセル済みエントリは生存数に対して
  増えすぎたら圧縮する（遅延パージだけだとヒープが「要求レート × タイムアウト」まで膨らむ。reactor も同じ。B-117）。
- プロキシ応答の中間バッファをリクエストごとに確保しない。ヘッダ解析回数を増やさない（B-72 第 2 弾）。
- HTTP/2 レスポンスヘッダは `Vec<(Bytes, Bytes)>`（F-165）。per-stream 固定費は接続あたり 1 回で済まないか先に考える（F-162）。
- `config::load_backend` はリクエストごとに呼ばれる。ルート単位の派生値は設定ロード時に `Route` の
  `#[serde(skip)]` フィールドへ解決し、ホットパスは `Arc` clone のみ（F-148/F-159、B-64）。
- UDS バックエンドの接続先表記は `ProxyTarget::conn_addr()` が唯一の入口（TCP では従来の `host:port` と 1 バイトも
  変えない）。同期プローブは `upstream::connect_probe`（F-170）。
- 上流のプール接続が応答の前に切れていたら、本文受信済みの冪等な要求だけ新しい接続で 1 回再送する（F-177）。
  POST 等は再送しない。再送用の複製は要求バッファのプールから取る（`clone()` しない）。
- 上流のプール接続はアイドル 1ms 以上のものだけ `MSG_PEEK` で生存確認して取り出す（B-93）。閾値を外して毎回確認すると
  プロキシ要求ごとに syscall が 1 本増える。平文は未読データも破棄、TLS は破棄しない（NewSessionTicket が残り得る）。

### reactor 固有

- epoll は `EPOLLONESHOT` + 再武装と確認用 `poll(2)` を省略しない（B-75）。kqueue の readiness ヒントは維持し、
  ヒントが無いときの `poll(2)` フォールバックを消さない（F-141）。
- fd ごとの待機者は `WakerSlot`（F-166）。accept は `accept_batch` で 1 周回に複数（reactor のみ。F-155）。
- Windows の WSAPoll はローカル `shutdown` を通知しないので、`shutdown` 時に読み取り待機者を起こす（B-88）。

### リスナー・ワーカー

- リスナーは必ず `server::create_listener`（UDS は `server::bind_unix_listener`）経由で作る（F-156/F-164）。
- ワーカーを増やすときは共有すべき状態（LB 状態・接続数カウンタ・ヘルスチェッカー）をリスナーにつき 1 個にする（F-156）。
- `[server].listen` は TLS 専用（平文は受けない）。平文 HTTP/1.1・h2c の計測・テストは `[server].listen_cleartext` に
  `enable_secure_redirect = false` を付けて使う（既定の `true` は https への 301 だけを返す。F-163/B-80/F-200）。
- `SO_ATTACH_REUSEPORT_CBPF` は classic BPF。使えるのは ancillary ロードのみ（B-76）。
- WASM の gRPC 呼び出しは専用スレッドで駆動する（tick スレッドへ戻さない。F-139）。
- キープアライブ待ちの接続は読み取りバッファを持たない（readiness を待ってから借りる）。mimalloc の
  `arena_eager_commit` は `main` 前（`.init_array`）で 0 にする（実行時に設定すると最初のアリーナに効かない。B-101）。
- 終了シグナルは unix では signal-hook で SIGINT / SIGTERM を受ける（ctrlc は termination feature 無しでは SIGTERM を
  捕まえず、有効にすると SIGHUP＝リロードも終了になる。B-115）。
- キープアライブで次の要求を待つ間（HTTP/1.1 の蓄積が空・HTTP/2 のストリームが無いとき）だけ
  `runtime::time::timeout_idle(.., true, ..)` を使う。graceful shutdown は `expire_idle_timeouts` でその待ちだけを
  満了させて閉じる（要求ごとの登録やフラグ監視を足さない。`Sleep` を `select!` に直接渡さず `fuse()` で包む。F-187）。
  HTTP/2 の接続アイドルタイムアウトもこの待ち（B-119）。HTTP/3 は停止中もメインループを回して GOAWAY → 処理中の要求が
  無くなった接続から CONNECTION_CLOSE（F-187）。
- ワーカースレッドは `runtime::block_on` を抜けた直後に `runtime::shutdown_thread()` を呼ぶ（残った接続タスクが
  スレッドローカルの破棄中に drop されると破棄済みの op テーブルに触れて abort する。B-118）。
- 動的設定は ArcSwap とリロード経路の不変条件を維持する。`unsafe` は最小限で、拡大時は不変条件をコメントで明示する。

### アロケータ・メモリ（F-183）

- アロケータ feature（`veil-alloc`・`mimalloc`・`jemalloc`・`system-allocator`）は排他的に 1 つ選ぶ。`full*` の
  feature セットに複数入れない（default の mimalloc と重なったときの優先順は veil-alloc > mimalloc > jemalloc）。
- unix の `full` / `full-container` のグローバルアロケータは `src/veil_alloc/`（`veil-alloc`、build.rs の
  `veil_alloc_active`）。高速パス（`heap::alloc` / `heap::free`）にロック・アトミック・分岐を増やさない。
  確保と解放の分類は `Layout` だけで決める（`classify` を両側で共有。ブロックヘッダや表を足さない）。
- アロケータ内の `thread_local!` は const 初期化かつ Drop 無しに限る（デストラクタ登録がアロケーションする）。
  スレッド終了は pthread キーのデストラクタで検出し、ヒープは解放せず次のスレッドへ引き継ぐ。
- アロケータの空き領域の返却はワーカーのアイドル点（accept の 1 秒タイムアウト・HTTP/3 のタイムアウト）の
  `alloc_tune::idle_tick` だけから行う（要求ごとの経路から呼ばない）。mimalloc は `mi_collect`、FreeBSD の
  libc jemalloc は `arena.<all>.decay`。
- mimalloc の起動時オプションは `.init_array` で設定し、公開されていない列挙値の添字は読み戻すテストで固定する。
  FreeBSD の libc jemalloc は `malloc_conf` シンボルで `retain:false`（FreeBSD の `madvise` は RSS を下げない）。
- ftlog の有界チャネルは全スロットを起動時に確保・常駐させる（`[logging] channel_size` の既定を増やさない）。
- TLS のセッション再開（`[tls] session_resumption`）のチケット鍵・キャッシュはプロセス共有（証明書リロードで
  作り直さない）。
- アロケータ・メモリの変更は `tools/perf/alloc_ab.sh`（BSD は `tools/perf/bsd/alloc_ab_bsd.sh`）で交互 A/B し、
  RSS の推移と `*_cpu_us_per_req` も見る。長時間の安定性は `alloc_soak.sh` の後半の傾きで判断する。
  リークは `leak_check.sh`（`alloc-stats` ビルドの生存ヒープ `veil_alloc_live_bytes`・fd・スレッド数を、接続チャーン・
  異常切断・リロードの反復ごとに採って傾きを見る。F-185）。

### API ライブラリ・制御プレーン（F-188〜F-196・F-04・F-05）

- ライブラリ用コード（`src/api/`、`handler`・`macro`・`openapi`・`tokio-interface`・`graphql`・`l4-handler`・`api`）は
  default・`full*` に入れない。本体側への追加は `cfg(feature = "handler")` の分岐だけにする（`tests/api/library_e2e.sh nonregression`
  が full バイナリに `veil::api`・inventory・async-graphql・tokio・pyo3 が無いことを検査する）。`handler` 時は `#[global_allocator]` を定義しない。
- ライブラリの設定は `config_override::apply_to_toml_str` のフックで合成する（既定 < ファイル < VDS < 環境変数 < インライン < ビルダ、
  ルート注入・具体性ソート・衝突検査）。リクエスト時にレジストリを引かない（`Backend::Handler` に解決済みの `Arc`）。
- ハンドラ・tokio の完了は `runtime::offload::await_completion`（結果スロット + 通知 fd）で受ける。スレッド越しに Waker を呼ばない。
  ブロッキングなハンドラ（同期 fn・C・Python）はハンドラ専用プール（`offload_handler`）で動かし、FS 用プールを使わない。
- VDS・ACME の通信は専用スレッドの `ctrl_http`（ブロッキング）で行い、データプレーンに触れない。ACME の HTTP-01 は専用リスナー。
- crates/（veil-macros・veil-capi・veil-python）はワークスペースメンバー。Dockerfile の cacher 段と `.dockerignore` に `crates/`・`benches/` が要る。
- Kubernetes の Service に `appProtocol` を付けない（Linkerd は `http` を HTTP/1.1 固定と解釈し h2c が壊れる。F-196）。

### プロトコル準拠・サンドボックス

- HTTP/3 のリクエストは `h3_check_field_section` で RFC 9114 §4.3.1 を検査する（quiche の h3 層は検査しない。B-100）。
  vendoring 版 quiche の差分（STREAM 詰め合わせ・予約ビット検査・rlib のみ）は `third_party/quiche/README.veil.md`。
- 不正なプリフェースや拒否する接続は、未読データを残して close しない（RST になり相手は GOAWAY を読めない）。
  送信側を閉じて上限付きで読み捨ててから閉じる（B-102）。
- seccomp の `ALLOWED_SYSCALLS` に syscall を足すときは番号を `libc::SYS_*` で確かめ、
  `test_allowed_syscalls_match_libc_numbers` にも足す（コメントと番号のずれで x86_64 の prctl が拒否されていた。B-103）。
- FreeBSD capability mode 下の上流接続は `connect_broker`（サンドボックス外の子プロセス）経由。本体が渡すのは
  起動時に確定した許可リストの **添字だけ**（アドレス・ホスト名・パスの文字列を受け付けるプロトコルにしない）。
  新しい接続経路を足すときは reactor の `connect*` か `connect_broker::connect_std_*` を通す（F-182）。
- container_security の判定は「本当に走ったか」を見る。h3spec の打ち切り・testssl の常時スキップ・サニタイザの
  起動失敗を ok と報告していた（B-100〜B-103 はすべてハーネスを直して初めて見つかった）。LeakSanitizer は
  終了時にしか走らず、veil の seccomp・Landlock・特権降格があると ptrace できずに落ちる（B-116）。

---

## 計測の原則（[docs/perf/README.md](docs/perf/README.md)・[tools/perf/README.md](tools/perf/README.md)）

- 性能改善は**交互 A/B** で確認する（時間をまたいだ比較は無意味）。両バックエンド・複数サイズ（3B と 54KB）で測る。
- 比較対象と**同じ条件**で測る（nginx 側にも `open_file_cache`、kTLS は両方オフ等）。キャッシュの有効化条件を満たす構成で測る。
- 蓄積するコストは定常状態で測る（`READ_TIMEOUT` を超える持続負荷の後）。allocs/req の削減とスループット改善は別の主張。
- ベースラインは `git worktree` で対象コミットを取り出してビルドする（`docker build` は作業ツリーをコピーする）。
- errors 列が跳ねている行はスループットより先に見る（0 rps は「測れていない」可能性）。
- `[profile.release]` は cargo 既定のまま。配布は `[profile.dist]`（fat LTO）。`panic = "abort"` は禁止（F-147）。

---

## 行動指針

1. 設計哲学・ホットパス規則・設計制約に整合するか確認する。
2. 触るコードの `cfg(feature)`・エラーハンドリング・ftlog・serde 設定型を先に読む。
3. 外部契約（設定キー・CLI・メトリクス名・プロトコル範囲）を変えたら同じ PR で `docs/guide/`（英日）と
   `examples/config.toml` を更新する。README は概要・Quickstart の範囲でのみ更新する。
4. 大きなロジックは専用モジュールへ。`entry.rs` は配線中心。
5. 挙動変更には単体 / 統合 / E2E のいずれかを追加・更新し、`cargo test` で実証する。
6. AI 成果物・調査メモ・ログは **`docs/artifacts/` にのみ** 置く。

## コーディング規約

- 既存に合わせる（多くは日本語のコメント）。英語への統一リファクタはしない。
- `cargo fmt`、`cargo clippy`。`#[allow(clippy::…)]` を論拠なく増やさない。**`allow(dead_code)` は禁止**。
- [clippy.toml](clippy.toml) の `disallowed-methods` がホットパスのブロッキングを検出する。正当な利用（offload 閉包内・
  専用スレッド・起動/リロード・テスト）は理由コメント付きの個別 `#[allow(clippy::disallowed_methods)]` で明示する。
- 依存追加は慎重に。`ftlog` のレベル・頻度は既存に揃える。

## 作業フロー

- 1 タスク = 小粒度（1 PR 1 目的）。feature 変更時は `--no-default-features` と関連 feature の組み合わせで確認する。
- バックログ: 機能は `docs/backlog/features/`、バグは `docs/backlog/bugs/`（1 件 1 md）。追加・ステータス変更時は
  **必ず同じ変更で [docs/backlog/backlog.md](docs/backlog/backlog.md) も更新する**。

## 禁止事項

- 依頼範囲外のドライブバイリファクタ、無関係ファイルの変更。
- ドキュメントを更新せず挙動・設定だけ変えること。
- 重い依存をデフォルト必須にすること。default を target 別に変えようとすること。
- 検証なしの `unsafe` 拡大、安易な `#[ignore]`（やむを得ない場合は理由を文書化）。
- `docs/artifacts/` 以外への AI 専用成果物の散乱。

---

## ビルド・テスト

```bash
cargo build --features full                                        # フル機能
cargo test --lib --bins --test integration_tests --features full   # 単体 + 統合（--lib を忘れると単体 0 件）
./tests/e2e_setup.sh test                                          # E2E（自動セットアップ・クリーンアップ）
VEIL_E2E_FEATURES="full,epoll" ./tests/e2e_setup.sh test           # reactor の E2E
```

- `http2` / `grpc` 系 feature が無いとコンパイルできない箇所がある。テストは十分な feature（`full`）で行う。
- **Linux の既定ビルドは `src/runtime/reactor/` を 1 行もコンパイルしない。** reactor に触れたら epoll の E2E を必ず回す（F-145）。
- **Windows / macOS / BSD は Linux のテストを全て通過する不具合を持ち得る。** クロスビルド
  （`packaging/scripts/build-cross.sh --target windows|macos`）に加え、可能なら実機・VM で `cargo test` と E2E を回す
  （BSD・Linux aarch64 は `tools/qemu/`。B-69/B-73/B-81/B-88）。
- `docker/Dockerfile.*` の cacher 段と builder 段は同じ `--profile dist` でビルドする（ずらすと依存を 2 回ビルドする。F-184）。
- コンテナイメージのビルド（`docker/Dockerfile.*`）は中で `cargo test --lib` を走らせる。単体テストは
  `tests/` 配下のフィクスチャを読まない（ビルドコンテキストに無い。証明書は `rcgen` で生成）、
  io_uring を必須にしない（ビルドのサンドボックスは io_uring を拒否する。可用性を見てスキップ）。
- E2E は直接 `cargo test` せず `tests/e2e_setup.sh` を使う。残存プロセスは `pkill -x veil` で止める（`-f` は自シェルを巻き込む）。
- 詳細は [docs/guide/testing.md](docs/guide/testing.md)・[tests/README.md](tests/README.md)。

---

## ディレクトリ要約

| パス | 役割 |
|------|------|
| `src/main.rs` / `src/lib.rs` / `src/entry.rs` | エントリ / クレートルート・公開 API / 起動配線 |
| `src/runtime/` | 独自ランタイム。共有（buf/io/offload）+ `uring/`（io_uring）+ `reactor/`（epoll/kqueue/WSAPoll） |
| `src/tls_writev.rs` | rustls 暗号文の `writev(2)` 直接送出（F-150） |
| `src/veil_alloc/` | veil 専用アロケータ（F-183、`veil-alloc`）。`src/alloc_tune.rs` はアイドル時回収 |
| `src/wasm_plugin_config.rs` | Proxy-Wasm プラグイン設定（`wasm` feature 非依存で常にコンパイル） |
| `tests/`、`benches/` | 単体以外のテスト・E2E・ベンチ |
| `examples/config.toml` | 設定リファレンス（全キー網羅） |
| `docs/guide/` | 利用者向けガイド（英語、`ja/` に日本語） |
| `docs/readme/` | 日本語 README |
| `docs/history/` | 過去の AGENTS.md 全文など、経緯と教訓のアーカイブ |
| `docs/perf/` | 最新のベンチマーク結果 |
| `docs/backlog/` | 機能・バグチケット（親は `backlog.md`） |
| `docs/artifacts/` | AI 成果物・一時ファイル（git 管理外） |
| `third_party/wasmtime/` | wasmtime 36.0.17（LTS）の vendoring（B-55 / B-79） |
| `third_party/quiche/` | quiche 0.24.9 の vendoring（F-172、STREAM フレーム coalescing） |
| `third_party/quinn-udp/` | テスト用 HTTP/3 クライアントの quinn-udp 0.5.14 の vendoring（NetBSD/aarch64 の CMSG アラインメント。B-62/F-176） |
| `docker/` | コンテナイメージ・Windows/macOS クロスビルド用 Dockerfile・共有アセット |
| `packaging/` | 配布物のビルド（[packaging/README.md](packaging/README.md)） |
| `tools/perf/` | nginx 比較の性能計測ハーネス |
| `tools/container_security/` | ファジング・カオス・h2spec/h3spec・セキュリティスキャン |
| `tools/qemu/` | BSD / Linux aarch64 の VM でのビルド・E2E（[tools/qemu/README.md](tools/qemu/README.md)） |
