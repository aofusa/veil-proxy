# F-198: tokio 互換レイヤーを tokio 無しで実現する（API ライブラリ専用）

**状態: 進行中（feat/f197-f200-impl）**

## 要望

- F-190 の「tokio インターフェース互換レイヤー」は tokio ランタイムを同梱して橋渡ししている。これを **tokio 無しを必須** に
  改め、veil の独自ランタイムの上で tokio の API と互換を高める。
- tokio を使う版（現在の実装のように最小限の tokio を引き込む橋渡し）を残す場合は **feature で切り替え** られるようにし、
  **API ライブラリの既定ビルドは tokio 無しの版** にする。
- tokio 無しで互換を高めたものは **tokio 自身のテストを使って** 互換性を確かめ、高める。
- **API ライブラリ専用**。veil のプロキシ用ビルド（default・`full*`）には一切含めない。
- **veil のランタイムの挙動を変えずに** 互換レイヤーを作れるかを確認する（→ 下の「ランタイムを変えずに作れるか」で
  「作れる」と結論）。

## 現状（調査結果）

| 項目 | 現状 |
|------|------|
| F-190 の実装 | `tokio-interface` feature が tokio（rt-multi-thread・time・sync・macros）を依存に入れ、`tokio_compat::run` は tokio のランタイムで Future を実行して結果を通知 fd で受け取る。`App::tokio_service` も tokio 上で動く |
| veil の Waker | index（32bit）+ 世代（32bit）を詰めた参照カウント無しの Waker。**所有ワーカースレッドでのみ wake される前提**（`runtime/uring/executor.rs` の「健全性」節）。別スレッドから wake すると、そのスレッドの `EXEC_STATE` の同じ index を起こしてしまう（不健全） |
| スレッド越しの完了 | `runtime::offload::await_completion`（F-188、`handler` feature）: 別スレッドが `Completer::complete` すると、結果スロットに入れて起点スレッドの通知 fd（Linux は eventfd、他は pipe）を叩き、起点スレッドは通知 fd の readable 待ち（io_uring は POLL_ADD、reactor は poller 登録）で起きる。取りこぼし・横取りは完了フラグの再確認と（reactor の）`wake_all_readers` で防いでいる |
| tokio の sync | `tokio::sync`（mpsc・oneshot・Mutex・Notify・Semaphore・watch・broadcast）はランタイム非依存だが、**別スレッドから wake する**（送信側が別スレッドにいる場合） |

→ tokio 互換の要は「(a) tokio の API 形状を veil のランタイムで提供する」と「(b) スレッド越しの wake を veil のタスクへ
健全に届ける」。

## feature の構成（API ライブラリ専用・切り替え式）

| feature | 内容 | tokio | `api` に含むか |
|---------|------|-------|---------------|
| `tokio-compat` | tokio 無しの互換レイヤー（下の `veil::tokio`・tower 互換・`App::tokio_service`）。**既定** | 依存しない | **含む（既定）** |
| `tokio-compat-bridge` | 現行 F-190 と同じ橋渡し（tokio のランタイムを同梱し、Future を tokio 上で実行して結果を通知 fd で受け取る）。tokio の内部に強く依存するクレート向け | 最小限（rt-multi-thread・time・sync） | 含まない（明示的に選ぶ） |

- 2 つは **排他**。両方有効は `compile_error!`（「どちらか一方を選ぶ」）。`api` は `tokio-compat` を含むので、
  ブリッジを使う利用者は `default-features = false, features = ["full", "api-core", "tokio-compat-bridge"]` のように
  `api` から `tokio-compat` を除いた集合（`api-core` = `handler`・`macro`・`openapi`・`graphql`・`l4-handler`・`grpc-handler`）を使う。
- 現行の `tokio-interface` feature は `tokio-compat-bridge` に改名（後方互換は持たない。ライブラリは未リリースの初版のため）。
- いずれも default・`full*`（プロキシのビルド）には入れない。`tests/api/library_e2e.sh nonregression` の検査に
  `veil::tokio`・`veil_tokio` のシンボルが無いことを足す。

## 設計（`tokio-compat`）

### 1. tokio の API 形状（`crates/veil-tokio`、tokio に依存しない）

veil のランタイムの上に tokio と同じパス・型名・シグネチャを提供する。使い方は 2 通り:

- **明示利用**: `use veil::tokio as tokio;`（`veil-tokio` を veil が再エクスポート）。
- **依存ツリー全体の置き換え**: `crates/veil-tokio/tokio-shim`（パッケージ名 `tokio`、版は 1.x）を
  `[patch.crates-io] tokio = { path = ".../tokio-shim" }` で当て、hyper・reqwest 等が使う tokio を差し替える。
  置き換えの可否はクレートごとに `docs/guide/library.md` の互換表で示す。

提供範囲（tokio の機能フラグと同じ単位で段階的に）:

| tokio のモジュール | veil での実現 |
|-------------------|--------------|
| `task::spawn`・`spawn_local`・`JoinHandle`・`JoinSet`・`yield_now`・`LocalSet` | 現在のワーカースレッドの executor に「互換タスク」（下の 2）として spawn。ランタイム外のスレッドから呼ばれたら tokio と同じく panic |
| `task::spawn_blocking`・`block_in_place` | `offload_handler`（ハンドラ用プール） |
| `time::{sleep, sleep_until, timeout, interval, Instant, Duration}` | `runtime::time`（同じスレッド内で完結） |
| `io::{AsyncRead, AsyncWrite, ReadBuf, AsyncReadExt, AsyncWriteExt, split, copy, BufReader, BufWriter, duplex}` | トレイトは tokio と同じ定義（poll ベース）。veil のレント型 I/O（所有バッファ）との橋は内部バッファで行う |
| `net::{TcpStream, TcpListener, UdpSocket, UnixStream}` | `runtime::tcp`・`runtime::udp`（`create_listener` 経由） |
| `sync::*` | tokio と同じ意味の実装を veil-tokio 内に持つ（wake は普通の `Waker` を呼ぶだけ。スレッド越しは下の 2 が受ける） |
| `select!`・`join!`・`try_join!`・`pin!` | `futures` のマクロに委譲（AGENTS.md F-173 の注意を文書化） |
| `main`・`test` 属性、`runtime::{Runtime, Builder, Handle}` | veil のランタイムで `block_on` |
| `signal`・`fs` | `signal`: signal-hook、`fs`: offload |
| `process` | 当面対象外（使う場合は `tokio-compat-bridge`） |

### 2. スレッド越しの wake（**ランタイムを変えない**）

互換タスクの包み（`CompatTask<F>`）が、内側の Future に **veil のものではない、スレッドセーフな互換 Waker** を渡す:

- 互換 Waker は `Arc<WakeSlot>` を持つ。`WakeSlot` = 所有スレッドの ID・その周回の `Completer<()>`（`await_completion` の送り手）・
  所有スレッド上で使う veil の Waker（タスク自身のもの。`Send` でないため所有スレッドでしか触らない）。
- `wake` が **所有スレッドから** 呼ばれたら、保存してある veil の Waker を呼ぶ（従来と同じ同一スレッドの wake）。
- `wake` が **別スレッドから** 呼ばれたら、`Completer::complete(())` を呼ぶだけ（結果スロット + 所有スレッドの通知 fd を叩く。
  veil の Waker には触らない）。
- `CompatTask::poll` は「内側の Future」と「その周回の `await_completion` の待ち」を両方 poll する。待ちが完了したら
  （= 別スレッドから wake された）次の周回の `Completer` を作り直して内側を poll し直す。
- これは F-188 の `offload`・`await_completion` と同じ仕組みの再利用で、executor・Waker の vtable・io_uring／reactor の
  処理は 1 行も変えない（使うのは既に存在する関数だけ。必要なら `pub(crate)` を `pub` にする可視性の変更のみ）。

### ランタイムを変えずに作れるか（確認結果）

**作れる。** 根拠と制約:

| 観点 | 結果 |
|------|------|
| 健全性 | veil の Waker は常に所有スレッドで呼ばれる（別スレッドの wake は通知 fd 経由で所有スレッドの I/O 完了として届く）。既存の不変条件を満たしたまま |
| 取りこぼし | `await_completion` と同じく「完了フラグを Release で立ててから通知 fd を叩き、待つ側は毎回フラグを確認する」。reactor の通知横取りは `wake_all_readers` で既に解決済み |
| 同一スレッド内の wake | 従来どおりの直接スケジュール（追加コストは `Arc` の参照と所有スレッド ID の比較 1 回） |
| コスト | 内側の Future が Pending を返した周回ごとに通知 fd の readable 待ちを 1 本張る（io_uring は SQE 1 個、reactor は poller 登録）。別スレッドから wake されたときは eventfd の write + 起床の分だけ遅延（数 µs）。いずれも互換タスクの中だけで、データプレーンには無い |
| 最適化 | 内側が Waker を複製しなかった周回（`Arc::strong_count` が 1 のまま）は、別スレッドから起こされ得ないので待ちを張らない |
| 制約 | tokio の work stealing（タスクのスレッド間移動）は無い（タスクは spawn したワーカーに固定）。`Handle::current()` を別スレッドへ持ち出して spawn する使い方は、その時点の所有スレッドへの spawn 依頼（通知 fd 経由）に変換する |

→ 当初案（ランタイムに `cross-thread-wake` を足して Waker をスレッド番号付きにする）は採らない。

### 3. tower 互換と `App::tokio_service`

- `tokio-compat` では tower の層・サービス（tower-http を含む）を veil のワーカー上でそのまま動かす。tokio の
  `time::sleep` 等に依存する層は `tokio-shim` の置き換えで veil の時計を使う。
- `App::tokio_service`（axum の Router 等）も `tokio-shim` で動くものはワーカー上で動かす。動かないものは
  `tokio-compat-bridge` を選ぶよう案内する。

### 4. 互換性の検証（tokio のテストを使う）

- tokio リポジトリの `tokio/tests/*.rs` と `tokio-util` の一部を、`tokio-shim` に置き換えて実行するハーネス
  `tools/tokio-compat/`（tokio のソースを固定版で取得し、`[patch]` で置き換えて `cargo test`）。
- 対象と合格基準を表で管理する（`time_sleep.rs`・`time_timeout.rs`・`time_interval.rs`・`io_*.rs`・`tcp_*.rs`・`udp.rs`・
  `sync_*.rs`・`task_join_set.rs`・`task_local.rs` 等）。work stealing・`rt_*`（ランタイム内部の挙動）に依存するテストは
  「非対応（理由）」として除外リストに入れる。
- 実ライブラリでの確認: hyper（サーバ・クライアント）、reqwest、tower-http の Timeout、sqlx（PostgreSQL・SQLite）を
  `tokio-shim` で動かす E2E。
- 合格率を `docs/guide/library.md` の互換表に記録し、版ごとに上げていく。
- `tokio-compat-bridge` は現行 F-190 の E2E をそのまま使う。

## 受け入れ条件

- `api` の既定ビルドで tokio が依存に無い（`cargo tree -e normal -i tokio` が空）。`tokio-compat-bridge` との排他が効く。
- tokio のテストの対象範囲で合格率を記録し、time・io・net・sync は 9 割以上。
- hyper・reqwest・tower-http・sqlx の E2E。
- veil のランタイム（`src/runtime/`）の挙動に変更なし（可視性の変更だけ）。プロキシ用ビルドにコードが入らない。

## リスク

- poll ベースの `AsyncRead` と veil のレント型 I/O の橋渡しでコピーが 1 回増える（互換レイヤーの対価として許容）。
- tokio の内部 API に依存するクレートは置き換えられない（互換表で明示し、`tokio-compat-bridge` を案内する）。
