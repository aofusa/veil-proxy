# F-170: Unix ドメインソケット（UDS）バックエンド接続対応

F-164 は **リスナー側**（`[server].listen` / `[server].h2c_listen` の `unix:<path>`）のみを
対象とし、「上流バックエンドへの UDS 接続」を明示的にスコープ外として別チケットへ送っていた。
本チケットはその残件で、**プロキシがバックエンドへ AF_UNIX で接続できるようにする**。

## 仕様

### 設定表記

| 用途 | 表記 | 例 |
|------|------|-----|
| HTTP プロキシ上流（`proxy_pass` 相当・`[[upstreams.servers]]`） | `http://unix:<socket-path>[:<path-prefix>]`<br>`https://unix:<socket-path>[:<path-prefix>]` | `http://unix:/run/app.sock`<br>`http://unix:/run/app.sock:/api` |
| L4 TCP 上流（`[[l4]].upstreams`） | `unix:<socket-path>` | `unix:/run/db.sock` |

- HTTP 側は **nginx の `proxy_pass http://unix:/path:/uri;` と同じ表記**を採る
  （既存ユーザーの知識をそのまま使える）。パスプレフィックスは
  ソケットパスの後ろの **最後の `:`** で区切り、区切り以降が `/` で始まるときのみ
  プレフィックスとして解釈する（省略時は `/`）。**ソケットパスに `:` は使えない**。
- L4 側は `[[l4]].listen` と同じ `unix:` 表記（`ListenAddr::parse` と一致）。
- `https://unix:...` は **UDS の上で TLS 終端する**バックエンド向け。SNI は既存の
  `sni_name`（未指定なら後述の論理ホスト名）を使う。
- 対応プラットフォームは **`cfg(unix)` のみ**。Windows では設定検証エラーで起動を拒否する。
- HTTP/3（QUIC/UDP）**上流**は対象外（QUIC は UDS 上で動かない）。ただし
  **HTTP/3 で受けたリクエストを UDS バックエンドへ中継する**のは対象（下流 H3 / 上流 UDS）。

### 論理ホスト名・ポート

UDS には host / port が無いが、既存コードは `ProxyTarget.host` / `.port` を
Host ヘッダ・SNI・プールキー・ログ・メトリクスに使う。そこで:

- `host` は **`localhost`** を既定値とする（`Host: localhost` を送る）。
  変更したい場合は既存の `[route.security].add_request_headers` で `Host` を上書きする。
- `port` は `0`、`is_default_port()` は **`true`**（Host ヘッダへ `:0` を付けない）。
- **プールキー・ログ・メトリクス・Consistent Hash のノード ID は
  `host:port` ではなく「接続アドレス表記」（`unix:<path>`）を使う。**
  TCP では `unix:` にならない＝従来と 1 バイトも変わらない
  （キーが変わるとプール再利用が静かに止まるため）。

## 設計

### 1. 接続アドレス表記の単一チョークポイント

`ProxyTarget` に UDS を表す情報を持たせ、**接続先文字列を作る唯一の入口**を用意する。

```rust
pub struct ProxyTarget {
    // 既存フィールド…
    /// UDS バックエンド（F-170）。`Some` のときソケットパスを持つ。
    /// `host`/`port` は Host ヘッダ・SNI・表示用の論理値（既定 "localhost" / 0）。
    pub unix_path: Option<Arc<PathBuf>>,
}

impl ProxyTarget {
    pub fn is_unix(&self) -> bool;
    /// 接続先表記。UDS なら `unix:<path>`、それ以外は従来どおり `host:port`。
    pub fn conn_addr(&self) -> HostPortStr;
}
```

- `Arc<PathBuf>` にするのは `load_backend` がリクエストごとに `ProxyTarget` を
  clone しうる（F-159 で `resolved_backend` にしたが、単一 URL プロキシの
  フォールバック経路は残っている）ため。参照カウント増分のみでディープコピーしない。
- **既存の `HostPortStr::new(&target.host, target.port)` 呼び出しをすべて
  `target.conn_addr()` に置き換える**（proxy.rs 6 箇所・http3_stream.rs 1 箇所・
  http3_server.rs・server.rs の `format!` 2 箇所）。TCP の結果文字列は不変。

### 2. ランタイム（`connect_str` の `unix:` 対応）

**すべての接続経路が `TcpStream::connect_str(addr)` か `connect_target()` を通る**ため、
ここで `unix:` 接頭辞を扱えば全プロトコル（HTTP/1.1・HTTP/2・h2c・HTTPS・HTTP/3→上流）が
同時に対応できる。

- `runtime::uring::tcp` / `runtime::reactor::tcp::unix`:
  - `TcpStream::connect_unix(path: &Path) -> ConnectUnix` を追加。
    - uring: `socket(AF_UNIX, SOCK_STREAM|SOCK_NONBLOCK|SOCK_CLOEXEC)` +
      `IORING_OP_CONNECT` に `sockaddr_un`。**新規オペコードは増やさない**
      （既存の `IORING_OP_CONNECT` をそのまま使う＝seccomp 許可リスト無変更）。
    - reactor: `connect(2)` の try-first → `EINPROGRESS` なら `register_write` で待機
      （既存 `Connect` と完全に同じ形。AF_UNIX は即時完了することが多い）。
    - **macOS には `SOCK_NONBLOCK`/`SOCK_CLOEXEC` が無い**（B-81）。
      `create_nonblocking_socket` の macOS 分岐（`socket` + `fcntl` 2 段）を必ず通す。
  - `connect_str` は `unix:` 接頭辞を検出したら `connect_unix` へ委譲する
    （TCP 経路の命令列は不変。`strip_prefix` の分岐 1 個のみ）。
- `runtime::reactor::tcp::windows`: `connect_str` は `unix:` を
  「このプラットフォームでは非対応」の `io::Error` で拒否する（B-69 の教訓＝
  windows.rs にも必ず同じ API を生やす）。

### 3. ヘルスチェック（同期プローブ）

`upstream.rs` の 3 プローブ（HTTP / TCP / gRPC）は `std::net::TcpStream` 直書きなので、
**`ProbeStream` 列挙（`Tcp(TcpStream)` / `Unix(UnixStream)`）** を導入して
`Read + Write` を実装し、`connect_probe(addr, timeout) -> io::Result<ProbeStream>` を
唯一の接続入口にする。`UnixStream::connect` にはタイムアウト付き API が無いため、
接続後に `set_read_timeout`/`set_write_timeout` を適用する（UDS の connect は
ローカルで即時完了するためタイムアウトの必要性が低い）。

### 4. L4 TCP 上流

`l4/proxy.rs` の `resolve_upstream_target()` が `SocketAddr` を返す単一チョークポイントに
なっているので、戻り値を `L4Endpoint { Tcp(SocketAddr), Unix(Arc<PathBuf>) }` にして
`IoUringTcpStream::connect` / `connect_unix` を選ぶ。L4 の UDP・
`[[l4]].listen` は対象外（本チケットは**上流**のみ）。

### 5. プラットフォーム別セキュリティ

- **Linux seccomp**: 追加 syscall なし（`socket`/`connect` は許可済み。AF_UNIX でも同じ）。
- **Linux Landlock**: UDS への `connect(2)` は Landlock の FS アクセス権では
  仲介されない（`LANDLOCK_ACCESS_FS_*` は open/rename 等が対象）。念のため
  バックエンドのソケットパスを読み書き許可パスへ追加する。
- **FreeBSD capsicum**: capability mode ではパス指定の `connect(2)` 自体が使えない
  （既存ドキュメントどおり、上流ありの構成で capsicum は使えない）。挙動変更なし。
- **OpenBSD pledge/unveil**: `PLEDGE_PROMISES` に **`unix`** を追加し、
  `collect_unveil_paths` が UDS バックエンドのソケットパスを
  `read_write_create`（`"rw"`）として収集する。**F-164 のリスナーパスも
  同時に収集していなかったので併せて追加する。**
- **macOS Seatbelt**: 既存プロファイルは `(allow system-socket)` 済みで追加不要。

## テスト

- 単体: `ProxyTarget::parse` の UDS 表記（プレフィックス有無・空パス・非 unix での拒否）、
  `conn_addr()` の TCP 側不変性、`connect_unix` の成功/`ENOENT`、
  `ProbeStream` のヘルスチェック、L4 エンドポイント解決。
- E2E: **veil 自身を UDS バックエンドとして起動する**（F-164 のリスナー実装をそのまま使う）。
  - `unix:/…/backend_uds_h2c.sock`（h2c）と `unix:/…/backend_uds_tls.sock`（TLS）の 2 本。
  - フロント veil から `http://unix:…`（h2c 上流）/ `https://unix:…`（TLS 上流）へ中継し、
    HTTP/1.1・HTTP/2・HTTP/3 の各クライアントで応答を検証する。
  - パスプレフィックス付き（`http://unix:…:/api`）・存在しないソケット（502）も検証する。

## 既知の制約（README に明記）

- UDS バックエンドの Host ヘッダは既定 `localhost`。`add_request_headers` で上書きする。
- `TCP_NODELAY` は AF_UNIX に存在しない（既存コードは `let _ =` で無視するので無害）。
- ヘルスチェックの `timeout_secs` は UDS の connect には適用されない（read/write のみ）。
- Windows は非対応（設定エラー）。

---

## 実装結果（2026-08-30）

### 設計からの差分

- `ProxyTarget.unix_path` の型は `Option<Arc<PathBuf>>` ではなく **`Option<Arc<str>>`**。
  接続先表記（`unix:<path>`）の組み立てに必要なのは UTF-8 文字列であり、`PathBuf` を
  経由すると `to_str()` の変換（と失敗時の panic 経路）が毎回挟まるため。
  設定は TOML の `String` 由来なので UTF-8 は型で保証できる。
- `ConnectUnix` は **`sockaddr_un` を Future 生成時に組み立てる**（当初案の
  「Future がパスを持つ」形は接続確立ごとに `PathBuf` の malloc が 1 回乗るため
  ホットパス絶対規則に反する）。構築失敗（パス長超過）は `Option<io::Error>` に
  積んで初回 `poll` で返す。成功パスで `format!` は実行されない。
- 同期プローブの共通化は `upstream::ProbeStream` + `connect_probe()`。
  `http3_server.rs` の同期 TLS バックエンド経路もこれを再利用する
  （列挙の重複実装を作らない）。**`connect_probe` はホスト名を `ToSocketAddrs` で
  解決する**: 旧実装は `addr.parse().unwrap_or_else(|_| 127.0.0.1:80)` で
  **ホスト名上流のヘルスチェックを黙って 127.0.0.1:80 へ向けていた**（本チケットで
  副次的に解消）。ここを `SocketAddr` パースだけにすると、`http3_server.rs` が
  旧 `std::net::TcpStream::connect(&str)` から引き継いでいる DNS 解決が消えて
  ホスト名バックエンドが壊れる（レビューで検出・修正済み）。
- `PoolKeyStr` の `host,port` ベースのコンストラクタは `#[cfg(test)]` へ移し、
  **F-170 以前の実装を凍結**して addr ベースの新実装と突き合わせる不変条件テストにした
  （`#[allow(dead_code)]` は使わない）。新実装へ委譲させると「同じコードを 2 回呼ぶだけ」の
  空虚なテストになるため意図的に実装を重複させている。

### 実装した範囲

| 経路 | 状態 |
|------|------|
| HTTP/1.1 クライアント → UDS バックエンド（平文 / TLS / h2c） | 対応 |
| HTTP/2 クライアント → UDS バックエンド | 対応 |
| HTTP/3 クライアント → UDS バックエンド（非同期経路・同期 TLS 経路とも） | 対応 |
| WebSocket プロキシ（平文 / TLS） | 対応 |
| バックグラウンド再検証（キャッシュ） | 対応 |
| ヘルスチェック（http / tcp / grpc） | 対応 |
| L4 TCP ストリームプロキシの上流 | 対応 |
| L4 UDP の上流 | **非対応**（設定検証と実行時の両方で拒否） |
| `[[l4]].listen` の UDS | **非対応**（F-164 と同じくスコープ外） |
| Windows | **非対応**（設定検証エラー） |

### プラットフォーム別セキュリティ

- Linux seccomp: 追加 syscall なし（`IORING_OP_CONNECT` を AF_UNIX で再利用）。
- Linux Landlock: UDS バックエンドのソケットパスを書き込み許可へ追加（保守的措置）。
- OpenBSD: pledge に `unix` promise を追加。unveil は **F-164 のリスナーパスも
  収集漏れだった**ため併せて追加（`config::collect_uds_socket_paths`）。
- macOS Seatbelt: 同じ収集関数で読み書き許可へ追加。
- FreeBSD capsicum: 上流接続ありの構成で使えない点は UDS でも同じ（コメントのみ追記）。

### 検証

- `cargo clippy --features full --all-targets -- -D warnings` / `--features "full,epoll"`: 警告ゼロ
- `cargo build --no-default-features`: 成功
- `cargo test --lib --features full`: **977 件成功** / `--features "full,epoll"`: **954 件成功**
- `cargo test --features full --test integration_tests`: **54 件成功**
- E2E（`./tests/e2e_setup.sh test`、io_uring）: **551 件成功・0 失敗**
- E2E（`VEIL_E2E_FEATURES="full,epoll" ./tests/e2e_setup.sh test`、reactor/epoll。
  F-145 の教訓により `src/runtime/reactor/` を触ったため必須）: **551 件成功・0 失敗**。
  F-170 の 7 件
  （HTTP/1.1・HTTP/2・HTTP/3 の各クライアント → UDS バックエンド、パスプレフィックス、
  存在しないソケットの 502、UDS 上流への TCP ヘルスチェック）を含む
- `packaging/scripts/build-cross.sh --target windows|macos`: B-69 / B-81 クラスの
  非 unix ビルド破壊がないことを確認

### 実装中に踏んだ自傷バグ（本チケット内で修正済み）

**`ProbeStream` がベクタード I/O を委譲していなかった。** rustls の
`ChunkVecBuffer::write_to` は暗号文チャンク列を最大 64 本の `IoSlice` で
`write_vectored` へ渡す。`Write` の既定実装は「最初の非空バッファ 1 本だけを書く」ため、
`writev(2)` へオーバーライド済みの `std::net::TcpStream` を `ProbeStream` で包んだだけで
**rustls の書き込みが停止し得る**。HTTP/3 の同期 TLS バックエンド経路がハングし、
`test_http3_buffering_spillover` が 20 秒でタイムアウトした。

**単体 977 件・統合 54 件・他の E2E 543 件はすべて通過する**タイプの不具合で、
E2E 1 件だけが落ちた。`main` で同テストが 0.2 秒で通ることを確認して退行と断定し、
コミット単位の二分（フェーズ1 は通る / フェーズ2 で落ちる）→
「`connect_probe` を元の `std::net::TcpStream::connect` に戻す」→
「`connect_timeout` は使うが `ProbeStream` で包まない」の 2 段階の切り分け実験で
ラッパそのものが原因であることを特定した。

`is_write_vectored` は unstable（`can_vector`）のため委譲できない。
`write_vectored` / `read_vectored` の委譲だけで解消することを実測で確認した。

**教訓: `Read`/`Write` を実装するラッパ型を既存のソケットに被せるときは、
`write_vectored` / `read_vectored` の委譲を必ず書くこと。** 既定実装は
「最初のバッファだけ」であり、`std::net::TcpStream` がオーバーライドしている
最適化を黙って無効化する。ホットパスの性能劣化だけでなく、
**ベクタード書き込みを前提にしたライブラリ（rustls）では停止に至る。**

### 実装中に発見した既存バグ（F-170 とは無関係・別チケット）

E2E で UDS バックエンドを veil 自身に喋らせたところ、**UDS とは無関係の既存バグ 2 件**を
踏んだ。どちらも TCP バックエンドでも同じように壊れる。

- **B-83**: 上流 TLS の ALPN で `h2` を提示するのに常に HTTP/1.1 を喋る。
  `http2_enabled = true` の HTTPS バックエンド（nginx/Envoy の既定構成、veil 同士の
  多段構成）へ中継すると常に 502。**既存 E2E のバックエンドが `http2_enabled` 未設定
  ＝ALPN を広告しないため一度も表面化していなかった。**
- **B-84**: HTTP/3 のストリーミングバックエンド経路（`http3_stream::run_backend_task`）が
  `use_h2c` を無視して HTTP/1.1 を送る。HTTP/3 → h2c 上流（= gRPC over HTTP/3 の中継）が
  502 になる。**HTTP/1.1・HTTP/2 クライアントからは同じルートが 200 で通る**ため
  気づきにくい。

F-170 の E2E はこの 2 件を踏まないよう構成した（TLS 用と h2c 用で UDS バックエンドの
プロセスを分ける / HTTP/3 の検証は TLS 上流で行う）。**この回避は E2E 側だけの措置で、
本体の修正は B-83 / B-84 で別途行う。**

### 実装中に踏んだ自傷バグ その2（本チケット内で修正済み）

**`connect_target` の UDS 分岐に `cfg(unix)` を付け忘れて Windows ビルドを壊した。**
`ProxyTarget::unix_path` は非 unix でも型としては存在する（値が常に `None`）ため、
`if let Some(path) = &target.unix_path { TcpStream::connect_unix(..) }` は
Windows でもコンパイル対象になり、`reactor::tcp::windows::TcpStream` に無い
`connect_unix` を呼んで `error[E0599]` になる。

**B-69 / B-73 / B-81 と同じクラスの 4 件目。** 単体 977 件・統合 54 件・
E2E 551 件（io_uring / epoll の両方）をすべて通過し、
`packaging/scripts/build-cross.sh --target windows` でのみ検出できた
（macOS は `reactor/tcp/unix.rs` を共用するため通ってしまう）。

**教訓: 「値が常に `None` だから安全」はコンパイルの話には通用しない。**
プラットフォーム限定の API を呼ぶ分岐は、条件がどうであれ `cfg` で切ること。
