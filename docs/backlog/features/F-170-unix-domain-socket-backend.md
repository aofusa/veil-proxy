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
