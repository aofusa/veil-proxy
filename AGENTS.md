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
  BSD 向けは `full-freebsd` / `full-openbsd` / `full-netbsd` という別 feature セットで表現する。
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
- quiche は `third_party/quiche` の vendoring 版を `[patch.crates-io]` で使う。差分は「複数ストリームの STREAM フレームを
  1 パケットへ詰める」1 行（upstream は fuzzing 時のみ。外すと小応答が 1 リクエスト 1 パケットに戻る。F-172）。
- 静的配信のパス解決に `canonicalize()` を使わない（dirfd 相対 `openat2`/`O_RESOLVE_BENEATH`。`openat2` は seccomp 許可必須。F-153）。
  絶対シンボリックリンクは Linux=`EXDEV`・FreeBSD=`ENOTCAPABLE` で拒否されるので、その場合だけ従来経路で再判定する（B-89）。
- FreeBSD の静的配信はヘッダも `sendfile(2)` の `sf_hdtr` で 1 syscall（平文 HTTP/1.1 のみ。TLS 経路に通すと平文漏洩。F-155）。
- HTTP/2・HTTP/3 の静的配信は `static_file_cache` と `open_file_cache` をセットで有効にして初めて offload ゼロになる（F-157）。
- HTTP/2 のインライン初回 poll（`spawn_inline`）は reactor 専用。io_uring 側の executor は変えない（F-158）。
- io_uring で I/O ごとにカーネルタイマーを張らない。`runtime::uring::timer` はユーザ空間の最小ヒープで、
  カーネルの `IORING_OP_TIMEOUT` は park 直前に 1 本だけ（B-72）。
- プロキシ応答の中間バッファをリクエストごとに確保しない。ヘッダ解析回数を増やさない（B-72 第 2 弾）。
- HTTP/2 レスポンスヘッダは `Vec<(Bytes, Bytes)>`（F-165）。per-stream 固定費は接続あたり 1 回で済まないか先に考える（F-162）。
- `config::load_backend` はリクエストごとに呼ばれる。ルート単位の派生値は設定ロード時に `Route` の
  `#[serde(skip)]` フィールドへ解決し、ホットパスは `Arc` clone のみ（F-148/F-159、B-64）。
- UDS バックエンドの接続先表記は `ProxyTarget::conn_addr()` が唯一の入口（TCP では従来の `host:port` と 1 バイトも
  変えない）。同期プローブは `upstream::connect_probe`（F-170）。
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
- `[server].tls_only` は既定 `true`。平文 h2c / HTTP/1.1 の計測・テストは `h2c_listen` か `tls_only = false` を使う（F-163/B-80）。
- `SO_ATTACH_REUSEPORT_CBPF` は classic BPF。使えるのは ancillary ロードのみ（B-76）。
- WASM の gRPC 呼び出しは専用スレッドで駆動する（tick スレッドへ戻さない。F-139）。
- 動的設定は ArcSwap とリロード経路の不変条件を維持する。`unsafe` は最小限で、拡大時は不変条件をコメントで明示する。

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
- E2E は直接 `cargo test` せず `tests/e2e_setup.sh` を使う。残存プロセスは `pkill -x veil` で止める（`-f` は自シェルを巻き込む）。
- 詳細は [docs/guide/testing.md](docs/guide/testing.md)・[tests/README.md](tests/README.md)。

---

## ディレクトリ要約

| パス | 役割 |
|------|------|
| `src/main.rs` / `src/lib.rs` / `src/entry.rs` | エントリ / クレートルート・公開 API / 起動配線 |
| `src/runtime/` | 独自ランタイム。共有（buf/io/offload）+ `uring/`（io_uring）+ `reactor/`（epoll/kqueue/WSAPoll） |
| `src/tls_writev.rs` | rustls 暗号文の `writev(2)` 直接送出（F-150） |
| `src/wasm_plugin_config.rs` | Proxy-Wasm プラグイン設定（`wasm` feature 非依存で常にコンパイル） |
| `tests/`、`benches/` | 単体以外のテスト・E2E・ベンチ |
| `examples/config.toml` | 設定リファレンス（全キー網羅） |
| `docs/guide/` | 利用者向けガイド（英語、`ja/` に日本語） |
| `docs/readme/` | 日本語 README |
| `docs/history/` | 過去の AGENTS.md 全文など、経緯と教訓のアーカイブ |
| `docs/perf/` | 最新のベンチマーク結果 |
| `docs/backlog/` | 機能・バグチケット（親は `backlog.md`） |
| `docs/artifacts/` | AI 成果物・一時ファイル（git 管理外） |
| `third_party/wasmtime/` | wasmtime 40.0.4 の vendoring（B-55） |
| `third_party/quiche/` | quiche 0.24.9 の vendoring（F-172、STREAM フレーム coalescing） |
| `docker/` | コンテナイメージ・Windows/macOS クロスビルド用 Dockerfile・共有アセット |
| `packaging/` | 配布物のビルド（[packaging/README.md](packaging/README.md)） |
| `tools/perf/` | nginx 比較の性能計測ハーネス |
| `tools/container_security/` | ファジング・カオス・h2spec/h3spec・セキュリティスキャン |
| `tools/qemu/` | BSD / Linux aarch64 の VM でのビルド・E2E（[tools/qemu/README.md](tools/qemu/README.md)） |
