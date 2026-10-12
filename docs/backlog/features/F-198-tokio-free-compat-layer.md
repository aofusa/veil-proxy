# F-198: tokio 互換レイヤーを tokio 無しで実現する（必要な所だけ tokio を引き込む）

**状態: 設計済み・未着手（design/f197-f200）**

## 要望

- F-190 の「tokio インターフェース互換レイヤー」は tokio ランタイムを同梱して橋渡ししている。これを **tokio を含めない**
  形に改め、veil の独自ランタイムの上で tokio の API と互換を高める。
- tokio 独自の機能のためにどうしても必要な場合だけ tokio の引き込みを許容する。
- tokio 無しで互換を高めたものは **tokio 自身のテストを使って** 互換性を確かめ、高める。

## 現状（調査結果）

| 項目 | 現状 |
|------|------|
| F-190 の実装 | `tokio-interface` feature が tokio（rt-multi-thread・time・sync・macros）を依存に入れ、`tokio_compat::run` は tokio のランタイムで Future を実行して結果を通知 fd で受け取る |
| veil の Waker | index（32bit）+ 世代（32bit）を詰めた参照カウント無しの Waker。**所有ワーカースレッドでのみ wake される前提**（`runtime/uring/executor.rs` の「健全性」節）。別スレッドから wake すると、そのスレッドの `EXEC_STATE` の同じ index を起こしてしまう（不健全） |
| スレッド越しの完了 | `offload`・`await_completion` は結果スロット + eventfd/pipe で起点スレッドへ戻す |
| tokio の sync | `tokio::sync`（mpsc・oneshot・Mutex・Notify・Semaphore・watch・broadcast）はランタイム非依存だが、別スレッドから wake する |

→ tokio 互換の要は「(a) tokio の API 形状を veil のランタイムで提供する」と「(b) スレッド越しの wake を健全にする」。

## 設計

### 1. `crates/veil-tokio`: tokio の API 形状の互換クレート（tokio に依存しない）

veil のランタイムの上に tokio と同じパス・型名・シグネチャを提供する。2 通りの使い方:

- **明示利用**: `use veil_tokio as tokio;`（ライブラリ側のコードを書き換えられる場合）。
- **差し替え**: 利用者の `Cargo.toml` で `[patch.crates-io] tokio = { package = "veil-tokio", ... }` は cargo の制約
  （パッケージ名・semver の一致）で使えないため、`crates/veil-tokio/tokio-shim`（パッケージ名 `tokio`、版は 1.x）を
  用意し `[patch.crates-io] tokio = { path = ".../tokio-shim" }` で依存ツリー全体の tokio を置き換える。
  置き換えの可否はクレートごとに `docs/guide/library.md` の互換表で示す。

提供範囲（段階的。tokio の機能フラグと同じ単位）:

| tokio のモジュール | veil での実現 |
|-------------------|--------------|
| `task::spawn`・`spawn_local`・`JoinHandle`・`yield_now`・`LocalSet` | 現在のワーカースレッドの executor に spawn（`Send` 境界は満たすので tokio と型互換）。ランタイム外のスレッドから呼ばれたら tokio と同じく panic（"must be called from the context of a veil runtime"） |
| `task::spawn_blocking`・`block_in_place` | `offload_handler`（ハンドラ用プール）。`block_in_place` は offload に置き換える |
| `time::{sleep, sleep_until, timeout, interval, Instant, Duration}` | `runtime::time`（io_uring はユーザ空間ヒープ + park 直前の 1 本、reactor はタイマーヒープ） |
| `io::{AsyncRead, AsyncWrite, ReadBuf, AsyncReadExt, AsyncWriteExt, split, copy, BufReader, BufWriter, duplex}` | トレイトは tokio と同じ定義（poll ベース）。veil のレント型 I/O（所有バッファ）との橋は内部バッファで行う |
| `net::{TcpStream, TcpListener, UdpSocket, UnixStream}` | `runtime::tcp`・`runtime::udp`（`create_listener` 経由）。poll ベースの I/O は reactor では readiness、io_uring では POLL_ADD + 非ブロッキング syscall |
| `sync::*` | tokio の sync は外部依存が少ないので、同じ意味の実装を veil-tokio 内に持つ（wake はスレッド越しの wake の仕組み 2 を通る） |
| `select!`・`join!`・`try_join!`・`pin!` | `futures` のマクロに委譲（ただし AGENTS.md の F-173 の注意どおり、I/O を複数待つ箇所には使わない旨を文書化） |
| `main`・`test` 属性 | veil のランタイムで `block_on` する属性マクロ |
| `runtime::{Runtime, Builder, Handle}` | veil のスレッド・パー・コアのランタイムを包む（`worker_threads` = ワーカー数） |
| `signal`・`process`・`fs` | `signal`: signal-hook、`fs`: offload、`process`: 当面対象外（必要になったら tokio を引き込む例外扱い） |

### 2. スレッド越しの wake を健全にする（`runtime` の改修、feature `cross-thread-wake`）

- veil-tokio（と F-197 の gRPC のチャネル等）が有効なときだけ、Waker を「所有スレッドの番号 + index + 世代」で作る
  （32bit 世代を 22bit 世代 + 10bit スレッド番号に分割。ワーカー 1024 本まで）。
- `wake` はスレッドローカルの番号と比較し、同じなら従来どおり即スケジュール。違えば所有スレッドの
  **リモート wake 受信箱**（ロックフリーの MPSC キュー）に (index, 世代) を積み、所有スレッドの通知 fd（eventfd / pipe）を叩く。
  所有スレッドは park 前とループの各周回で受信箱を空にする（io_uring は通知 fd の POLL_ADD、reactor は poller 登録）。
- feature が無効なビルド（veil バイナリ）の Waker・executor は 1 ビットも変えない（`cfg` で分ける）。有効時の追加コストは
  wake 1 回あたりスレッドローカルの比較 1 回。交互 A/B で確認する。

### 3. tokio を引き込む例外

- veil-tokio で提供しない API（`process`・`io_uring` 固有の tokio-uring 等）や、tokio のランタイムに強く結合した
  クレートは、従来の F-190 の橋渡し（tokio ランタイムで実行して通知 fd で受け取る）を **別 feature
  `tokio-bridge`** として残す。`tokio-interface`（tower 互換）からは tokio を外す。
- `App::tokio_service` は veil-tokio で動くなら veil のワーカー上で、そうでなければ `tokio-bridge` で動かす。

### 4. 互換性の検証（tokio のテストを使う）

- tokio リポジトリの `tokio/tests/*.rs` と `tokio-util` の一部を、`tokio-shim` に置き換えて実行するハーネス
  `tools/tokio-compat/`（tokio のソースを固定版で取得し、`[patch]` で置き換えて `cargo test`）。
- 対象と合格基準を表で管理する（例: `time_sleep.rs`・`time_timeout.rs`・`time_interval.rs`・`io_*.rs`・`tcp_*.rs`・
  `udp.rs`・`sync_*.rs`・`task_join_set.rs`）。マルチスレッドの work stealing・`rt_*`（ランタイム内部の挙動）に依存する
  テストは「非対応（理由）」として除外リストに入れる。
- 実ライブラリでの確認: hyper（サーバ・クライアント）、reqwest、tower-http の Timeout、sqlx（PostgreSQL・SQLite）を
  `tokio-shim` で動かす E2E。
- 合格率を `docs/perf/` と同様に `docs/guide/library.md` の互換表に記録し、版ごとに上げていく。

## 受け入れ条件

- `tokio-interface`（tower 互換）と veil-tokio が tokio を依存に含まない（`cargo tree -e normal -i tokio` が空）。
- tokio のテストの対象範囲で合格率を記録し、time・io・net・sync は 9 割以上。
- hyper・reqwest・tower-http・sqlx の E2E。veil バイナリの性能・コードに変化なし（`cross-thread-wake` 無効）。

## リスク

- poll ベースの `AsyncRead` と veil のレント型 I/O の橋渡しでコピーが 1 回増える（ハンドラ側の互換レイヤーの対価として許容し、
  データプレーンには入れない）。
- tokio の内部 API（`tokio::runtime::Handle::current()` の振る舞い等）に依存するクレートは置き換えられない。互換表で明示する。
