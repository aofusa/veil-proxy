# F-28: monoio 削除・カスタム io_uring 実装

## 概要

現在 `monoio = { version = "0.2.4" }` に依存しているランタイム層を完全に削除し、`libc` と `bytes` のみを使った独自の io_uring 実装に置き換える。thread-per-core 設計を維持しつつ、`IORING_REGISTER_RESTRICTIONS` によるセキュリティ強化と `IORING_OP_SPLICE` によるゼロコピーを実現する。

## 現状

- `monoio 0.2.4` が io_uring の低レベル API を抽象化してしまい `IORING_REGISTER_RESTRICTIONS` を適用できない
- monoio の所有権ベース I/O モデルがゼロコピー実装の妨げになっている
- `IORING_OP_SPLICE` が直接使えない

## 改修内容

### src/runtime/ モジュール構成

```
src/runtime/
├── mod.rs       - 公開 API、スレッドランタイム起動
├── ring.rs      - io_uring リング管理（setup/enter/register、SQE/CQE raw 操作）
├── executor.rs  - シングルスレッド非同期エグゼキュータ（Waker 実装）
├── tcp.rs       - 非同期 TcpListener・TcpStream
├── timer.rs     - タイムアウト管理（IORING_OP_TIMEOUT）
└── buf.rs       - IoBuf/IoBufMut トレイト
```

### 使用オペコード

| オペコード | 用途 |
|-----------|------|
| `IORING_OP_ACCEPT` | 接続受け入れ |
| `IORING_OP_RECV` / `IORING_OP_READ` | 読み取り |
| `IORING_OP_SEND` / `IORING_OP_WRITE` | 書き込み |
| `IORING_OP_CONNECT` | 接続 |
| `IORING_OP_CLOSE` | クローズ |
| `IORING_OP_TIMEOUT` | タイムアウト |
| `IORING_OP_CANCEL` | キャンセル |
| `IORING_OP_SPLICE` | ゼロコピースプライス（kTLS 用） |

### セキュリティ設定

- `IORING_REGISTER_RESTRICTIONS` で使用するオペコードのみ許可
- `IORING_SETUP_SQPOLL` を設定で有効/無効を選択可能
- `IORING_SETUP_IOPOLL` による高性能ポーリングモードサポート

### io_uring raw syscall（libc 使用）

```rust
const IORING_SETUP: i64 = 425;   // io_uring_setup(2)
const IORING_ENTER: i64 = 426;   // io_uring_enter(2)
const IORING_REGISTER: i64 = 427; // io_uring_register(2)
```

### TcpStream API（monoio 互換）

- `TcpListener::bind(addr)` → バインド + リッスン
- `TcpListener::accept()` → `Future<Output=(TcpStream, SocketAddr)>`
- `TcpStream::connect(addr)` → `Future`
- `TcpStream::read(buf)` → `Future<Output=(Result<usize>, buf)>` （所有権ベース）
- `TcpStream::write(buf)` → `Future<Output=(Result<usize>, buf)>` （所有権ベース）
- `TcpStream::readable().await` / `TcpStream::writable().await`
- `TcpStream::as_raw_fd()` / `TcpStream::set_nodelay(bool)`

### エグゼキュータ設計

- 各ワーカースレッドが独自の io_uring リング + エグゼキュータを持つ
- CQE の `user_data` で Future を特定して wake
- `spawn(future)` / `block_on(future)` API

## 変更ファイル一覧

| ファイル | 変更内容 |
|---------|---------|
| `Cargo.toml` | `monoio` 依存を削除 |
| `src/runtime/` | 新規作成（全モジュール） |
| `src/main.rs` | `RuntimeBuilder` → `runtime::block_on` |
| `src/pool.rs` | `monoio::buf::*` → `runtime::buf::*` |
| `src/ktls_rustls.rs` | `monoio` → 新 runtime |
| `src/simple_tls.rs` | `monoio` → 新 runtime |
| `src/http2/` | `monoio` → 新 runtime |
| `src/http3_server.rs` | `monoio` → 新 runtime |
| `src/security.rs` | io_uring 制限のコメントを実装に変更 |

## 受け入れ条件

- [ ] `monoio` が `Cargo.toml` から削除されている
- [ ] `cargo build --features "full"` が通る
- [ ] `cargo test --bins --test integration_tests --features "full"` が通る
- [ ] `IORING_REGISTER_RESTRICTIONS` が適用されている
- [ ] `cargo clippy --features "full"` が警告なし

## 依存・リスク

- libc クレートの `io_uring_sqe` / `io_uring_cqe` 型定義（libc 0.2.178+ で利用可能）
- カーネル 5.6+ で io_uring の基本オペコードが利用可能
- `IORING_REGISTER_RESTRICTIONS` には カーネル 5.10+ が必要

## ブランチ

`feat/custom-iouring-impl`

## 優先度

P1（本タスクの核心）

---

## 完了メモ（2026-06-23）

カスタム io_uring ランタイム（`src/runtime/`）は実装済みだったが、**`IORING_REGISTER_RESTRICTIONS` が 3 つのバグで完全に無効化**されていたため修正した:

1. `IoUringRestriction` 構造体が 20 バイトでカーネルの 16 バイトと ABI 不一致（union メンバを別フィールド化 + `resv` を u32 化）。
2. リングを `IORING_SETUP_R_DISABLED` なしで生成 → `apply_restrictions` が `-EBADFD` で失敗し debug ログに握り潰し。
3. `enable_rings()` 未呼び出し（かつ制限後は ENABLE_RINGS 自体の register_op 許可が必要）。

「R_DISABLED 生成 → 制限登録（ENABLE_RINGS 許可含む）→ ENABLE_RINGS」の正しいシーケンスに修正。許可外オペコードが `-EACCES` で拒否されること・構造体が 16 バイトであることを `src/runtime/ring.rs` の単体テストで検証。許可オペコード = ACCEPT/CONNECT/RECV/SEND/POLL_ADD/TIMEOUT/CLOSE/SPLICE/POLL_REMOVE/ASYNC_CANCEL/NOP。

## 完了メモ追記（2026-06-28）: サーバが応答できない致命バグ群を修正

E2E（`tests/e2e_setup.sh test`）を実機で通すと、独自ランタイムには「接続を accept しても
応答できない」致命バグが複数残っていた（エグゼキュータ修正までハンドラが走らず未検出だった）。
修正後 E2E は 388/389 通過（残り 1 件は 200 同時接続の負荷フレーキーでテスト毎に入れ替わる）。

- **エグゼキュータ不一致**: `block_on_with_config` が `Executor::new()` の別キューを使い、
  `spawn()` 先のスレッドローカルエグゼキュータと不一致。accept ループが spawn した
  `handle_connection` が永遠にポーリングされず無応答。`current_executor()` を使うよう修正。
- **`storage_to_sockaddr` IPv4 バイトオーダ二重変換**: accept が返すクライアント IP が逆順。
- **Accept の cancel-on-drop**: `timeout(1s, accept())` の drop でバッファ UAF・孤立 accept の
  リスク。OpTable に detach 機構（ガード保持 + ASYNC_CANCEL）を追加。
- **プロトコル検出が TLS を破損**: h2c_enabled 時の `detect_protocol_with_buffer` が io_uring
  RECV でバイトを消費して持ち回す際に破損 → 後続 TLS が `InvalidContentType` で失敗。
  `libc::recv` の MSG_PEEK（非消費）方式に変更。
- listen backlog 128→1024（接続バースト耐性）。
