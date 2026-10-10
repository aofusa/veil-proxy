# F-145: ホットパスの syscall 削減（reactor readiness probe / SendFile base_path 二重解決）

## 背景・実測根拠

FreeBSD aarch64 実機の DTrace で、veil が TLS HTTP/1.1 で 54KB 静的ファイルを配信中
（約 8 秒間で約 174,000 リクエスト）の syscall をリクエストあたりに正規化した実測値:

| syscall | 回数/リクエスト |
|---------|-----------------|
| `openat` | 1 |
| `close` | 1 |
| `sendfile` | 1 |
| `write` | 2 |
| `__realpathat` | 2 |
| `fstatat` | 3 |
| `poll` | 1.7 |
| `kevent` | 0.74 |
| `_umtx_op` | 3.4 |
| `aio_read` | 1 |
| `aio_write` | 1 |
| `aio_error` | 2 |
| `aio_return` | 2 |

合計で約 20 syscall/リクエスト。nginx は同条件で約 5〜6 syscall/リクエストに収まる。
3 バイトの極小ファイルでは veil は 83k rps に対し nginx は 384k rps（0.22 倍）で、
1 リクエストあたりのオーバーヘッドが支配的なボトルネックであることが確認された。

本チケットは、上記実測のうち **原因が明確で副作用が小さい 2 点**（`poll(2)` の投機的
プローブ、SendFile の base_path 二重解決）のみを対象にする。`aio_*` 系の削減やコンテンツ
キャッシュ導入など、設計変更や実機再検証が必要な残りの項目は別チケットへ切り出す
（本チケットでは着手しない）。

## 実施した改修

### 1. reactor readiness Future の投機的 `poll(2)` プローブを削除

`src/runtime/reactor/tcp/unix.rs` の `Readable`/`Writable`/`ReadableFd`/`WritableFd` は、
`register_read`/`register_write` で待機登録する前に `libc::poll(&mut pfd, 1, 0)` で
即時 readiness を確認していた。呼び出し箇所監査（全呼び出しを洗い出し、非ブロッキング
`read`/`recv`/`write`/`send`/`sendto`/`sendmmsg` が `EAGAIN`/`WouldBlock` を返した **後**
にのみ await されているかを確認）の結果:

- `src/pool.rs`（kTLS raw read/write）、`src/proxy.rs`（body splice 転送・kTLS sendfile
  リトライ・H2C プロトコル判別後の再試行ループ）、`src/http3_stream.rs`（バックエンド TCP
  read/write）、`src/ktls_rustls.rs`/`src/simple_tls.rs`（TLS ハンドシェイク・平文
  read/write の全経路）、`src/udp/socket.rs`/`src/runtime/udp.rs`（recv/send/GRO/GSO/
  mmsg 系）、`src/http3_server.rs`（H3→バックエンド TCP プロキシ）、`src/l4/proxy.rs`
  （L4 splice 転送）、`src/runtime/reactor/sendfile.rs`（FreeBSD `sendfile(2)` の
  `EAGAIN` リトライ）は、**すべて** try-first パターン（非ブロッキング呼び出し →
  `WouldBlock` → readiness 待機 → リトライ）に従っていた。
- **例外 1 件を発見**: `src/proxy.rs` の H2C/TLS/HTTP1.1 プロトコル判別ループ
  （`detect_protocol` 相当、旧行番号 3890 付近）は、ループ先頭で
  `timeout(remaining, stream.readable()).await` を呼んでから `MSG_PEEK` で覗き見る
  「先に readable 待ち→ peek」パターンであり、初回イテレーションでは非ブロッキング
  試行なしに `readable()` を呼ぶ。ただし `MSG_PEEK` はソケットバッファを消費しない
  ため副作用は無く、後述の正しさの論拠（登録時点で既に readable ならカーネルが即座に
  イベントを報告する）がそのまま当てはまるため、`poll(2)` プローブ削除後もハングしない
  （最悪でもイベントループが 1 ターン余分に回るだけ）。
- `src/runtime/offload.rs` の `OffloadWait`（reactor バックエンドの offload 完了待機）
  は `Readable`/`Writable`/`ReadableFd`/`WritableFd` とは別の独自 Future であり、
  eventfd 相当の完了カウンタを複数タスクで共有 drain する構造上の理由から、
  自前の `poll(2)` プローブ（コメントで根拠を明記済み）を今回の変更対象外として維持した。

これらの呼び出し元は例外なく「非ブロッキング syscall が `EAGAIN` を返した直後」に
readiness Future を await するため、登録前の `poll(2)` はほぼ常に「not-ready」を返す
だけの無駄な syscall だった（実測: 約 1.7 回/リクエスト）。

**正しさの論拠**: kqueue の `EV_ADD` registration・epoll のレベルトリガ登録は、いずれも
登録時点で fd が既に readable/writable であれば直ちにイベントを報告する。そのため
「登録した瞬間に既に ready だった」ケースでも起床が失われることはなく、最悪でも
イベントループが 1 ターン余分に回るだけで済み、ハングしない。kqueue バックエンドの
登録は F-141 の changelist バッチ化により次の `kevent()` にまとめて渡されるため、
`poll(2)` 削除は純粋に −1 syscall（追加 syscall なし）になる。

`#[cfg(veil_poller_kqueue)] take_read_hint(...)`（F-141、consume-once の read hint）
は syscall コストが無いため変更していない。

Windows の `src/runtime/reactor/tcp/windows.rs` にも同型の `WSAPoll` プローブ
（`poll_ready_now`）を持つ `Readable`/`Writable`/`ReadableFd`/`WritableFd` があるが、
`veil_poller_wsapoll` は cfg 発行のみで実装は未着手（AGENTS.md 記載）であり、この
バックエンドは Windows 上でコンパイル・実行して検証する手段が本セッションには無いため
**変更しなかった**。呼び出し元は unix.rs と共有（proxy.rs 等はクロスプラットフォーム）
のため try-first の前提は同様に成立すると考えられるが、未検証のまま変更するのは
避けた。

### 2. SendFile の base_path 二重解決を config ロード時に一度だけへ集約

ディレクトリルートの File ルートでは、コンテナメント検査（`file_info.canonical_path
.starts_with(base_info.canonical_path)`）のためだけに、リクエストごとに
`cache::get_file_info_with_config(base_path, ...)` を実ファイルとは別にもう一度呼んで
いた。`open_file_cache` 無効時（デフォルト）はこれが毎回 `runtime::offload` 経由の
ワーカースレッド往復 + `canonicalize`（`__realpathat`）+ `metadata`（`fstatat`）になる
（実測: `__realpathat` 2 回/リクエストのうち 1 回、`fstatat` 3 回のうち 1 回、
`_umtx_op` 3.4 回の一部がこの往復に起因）。

base_path はロード済み設定に対して不変なので、`config::load_backend`（cold path）で
一度だけ `std::fs::canonicalize` して `Backend::SendFile` の新フィールド
`Option<Arc<Path>>`（`canonical_base`）として保持し、HTTP/1.1・HTTP/2・HTTP/3 の
静的配信経路すべてでこれを使い回す（`cache::sendfile_base_contains` に集約）。

- 失敗時（起動時点でディレクトリ未作成等）は起動を失敗させず `None` にして警告ログを
  一度だけ出し、リクエスト時は生の `base_path` へフォールバックする。`full_path` は
  常に `base_path` を起点に join して構築されるため、この場合の比較は恒真（許可）に
  なる。従来コードもこの場合（base_path の解決失敗）は封じ込め検査自体をスキップ
  （実質許可）していたため、結果として同じ「許可」に帰着し、運用上ディレクトリが
  後から作られても動作し続ける。
- **FreeBSD capability mode（`cap_enter`、F-123）**: 実際のパストラバーサル封じ込めは
  dirfd 相対 `openat`/`fstatat` の `O_RESOLVE_BENEATH` が担う。この経路が有効な間
  （`security::capsicum::static_serving_active()`）は `file_info.canonical_path` 自体が
  生パス（`full_path` そのもの）を返すため、`full_path` が常に `base_path` の join で
  構築される以上 `starts_with` 比較は必ず真になる（cap_enter 前と結果は変わらない）。
  `sendfile_base_contains` はこの atomic フラグ（追加 syscall 無し）を見て、capability
  mode 中は比較そのものを省略する。`config::load_backend` の `canonicalize` は
  cap_enter **前**（設定ロード時）に実行されるため、capsicum 側の dirfd 登録
  （`security::capsicum::init_static_dirfds`、こちらも cap_enter 前）や
  `stat_static`/`open_static_ro` の相対化ロジックには一切手を入れていない。

### 変更ファイル

- `src/runtime/reactor/tcp/unix.rs`（Change 1）
- `src/cache/mod.rs`（`sendfile_base_contains` 追加、`cache` feature の有無に依存しない
  共通ヘルパとして feature ゲート外に配置）
- `src/config.rs`（`Backend::SendFile` に `Option<Arc<Path>>` フィールド追加、
  `load_backend` で config ロード時に一度だけ canonicalize）
- `src/proxy.rs`（`h2_sendfile`・HTTP/1.1 `handle_sendfile` を `canonical_base` 経由に変更）
- `src/http3_server.rs`（`Backend::SendFile` 分解のタプル arity 修正。**HTTP/3 の
  `handle_sendfile` は元々 `cache::get_file_info_with_config` によるキャッシュ・
  canonical パス封じ込め検査を一切行っておらず（`std::fs::read` を offload するのみの
  簡易実装）、削減対象の「二重解決」が存在しなかった**。そのためコンパイルを通す
  ための最小修正のみ行い、新たな封じ込め検査の追加はスコープ外として見送った
  （既存の HTTP/3 静的配信の挙動・受け入れ条件を変えないため。追加するなら別チケット）。

## 見送った項目（別チケット）

- `aio_read`/`aio_write`/`aio_error`/`aio_return` の削減（POSIX AIO 経路の再設計が必要）。
- コンテンツキャッシュ（`open_file_cache`/レスポンスボディキャッシュ）の既定有効化・
  設計変更によるファイル I/O 自体の削減。

---

## 【重要】変更 1（投機的 `poll(2)` プローブ削除）は **撤回した**（2026-08-07）

**この最適化は実装・検証の結果 revert し、`src/runtime/reactor/tcp/unix.rs` は
F-145 適用前の状態へ完全に戻した。** 変更 2（base_path の解決を config ロード時
一度きりへ集約）はそのまま残している。

### 撤回に至った実測

| reactor の版 | h2c リクエスト（`curl --http2-prior-knowledge`） |
|---|---|
| F-145 適用前（元の実装） | **200 OK** |
| F-145 第1版（プローブ削除のみ、c4ab101） | **ハング**（応答なし） |
| F-145 第2版（`registered` フラグ + 書き込み側プローブ復活、ea23ae5） | **なおハング** |
| プローブ削除を全て revert | **200 OK** |

途中で実在するバグを 2 つ発見・修正した:

1. **`Writable`/`WritableFd` から `Poll::Ready` を返す経路が消滅していた。**
   プローブが唯一の完了経路だったため、起床しても再登録して `Pending` を返し続ける。
   FreeBSD 実測で 54KB 応答が送信バッファ充填時点で永久停止した（wrk -c1 で 0 rps）。
2. **`Readable` は読み取りヒントが 0 の EOF イベントで完了できない**（同じ構造の潜在バグ）。

しかし **2 つを修正してもなお h2c がハングする**（3 つ目の未解明の不具合が残っている）。
ランタイムのホットパスで原因を説明できない変更を残すべきではないため、
プローブ削除そのものを撤回した。

### 再挑戦する場合の前提条件

- **先に `VEIL_E2E_FEATURES="full,epoll"` の E2E を通常の検証手順へ組み込むこと。**
  今回の一連の不具合は「最初の 1 リクエストでサーバがハングする」ほど重篤だったにも
  かかわらず、単体 816 件・統合 53 件・E2E 541 件のすべてを通過した。Linux の既定
  ビルドは io_uring バックエンド（`veil_rt_uring`）であり、reactor のコードを
  **1 行もコンパイルしない**ためである。reactor は BSD 全種・macOS・
  コンテナ（`full-container`）で使われる本番経路であり、テストの空白地帯だった。
- プローブは「fast path」ではなく **完了経路そのもの**として機能している可能性がある。
  削除する場合は 4 つの Future すべてについて `Ready` へ到達する経路を個別に検証すること。

### 撤回後に残る成果

- 変更 2（`Backend::SendFile` の `canonical_base`）: リクエストごとの
  `__realpathat` 1 回・`fstatat` 1 回・offload スレッド往復 1 回を削減。
- B-63（`aio` の既定除外）: こちらが FreeBSD 性能改善の本命で、小レスポンスの
  HTTP/1.1 TLS +77.6%・L4 TCP 約 5.5 倍・HTTP/2 の完全停止解消。

## 完了（2026-10-10）

変更 2（SendFile の canonical_base 事前解決）は採用済みで、B-64・F-159・B-109 で拡張された。変更 1 は撤回済みで、
再挑戦の前提（epoll E2E の常設）は AGENTS.md に入っている。別チケット送りの項目は B-63（aio）と F-157（キャッシュ）で
扱う。以上で本チケットは完了（変更 1 は撤回）とする。
