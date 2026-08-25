# F-164: Unix ドメインソケット（UDS）リスナー対応

## 仕様

- `[server].listen` / `[server].h2c_listen` / `[[l4]].listen`（TCP のみ）に
  **`unix:<path>`** 形式を受理する。例: `listen = "unix:/run/veil/https.sock"`。
- 対応プラットフォームは **`cfg(unix)` のみ**。Windows では設定エラーとして起動を拒否する
  （「この環境では unix ソケットは非対応」と明示）。
- HTTP/3（QUIC/UDP）は UDS 非対応（設計上不可）。`[http3].listen` は従来どおり `host:port` のみ。

## 設計

### アドレス表現

新モジュール `src/listen_addr.rs`:

```rust
pub enum ListenAddr { Tcp(SocketAddr), #[cfg(unix)] Unix(PathBuf) }
```

- `ListenAddr::parse(&str)`: `unix:` 接頭辞なら UDS、それ以外は `SocketAddr` としてパース。
- `Display` は元の表記を保つ（ログ・メトリクスの見た目を壊さない）。
- 既存の `LoadedConfig::listen_addr: String` はそのまま（文字列を保持）とし、
  `entry.rs` でのパースを `SocketAddr::from_str` から `ListenAddr::parse` に置き換える。
- `HTTPS_REDIRECT_PORT` は TCP のときだけ更新する（UDS ではリダイレクト先ポートが無いため
  既定 443 のまま。README に明記）。

### bind とワーカー分散

TCP は `SO_REUSEPORT` でワーカーごとに bind するが、**AF_UNIX に SO_REUSEPORT は無い**。
したがって UDS は次のようにする。

1. ワーカー spawn 前に **1 回だけ** bind + listen する（`server::bind_unix_listener`）。
   - 既存パスが**ソケットである場合に限り** `unlink` してから bind（stale socket 対応）。
     通常ファイル・ディレクトリなら安全側に倒してエラー。
   - `[server].unix_socket_permissions`（既定 `"0660"`）で bind 直後に `fchmodat`/`chmod`。
   - FreeBSD capsicum のリスナー権利制限は TCP と同じく適用する。
2. 各ワーカーは共有した listen fd を `dup(2)` して自分の `TcpListener` にする
   （fd はワーカーごとに独立に close される）。カーネルが accept を分散する。
3. 終了時に socket ファイルを `unlink` する（graceful shutdown 経路）。

### ランタイム

`runtime::TcpListener` を AF_UNIX でも使えるようにする（型は増やさない＝ホットパス無変更）。

- `uring/tcp.rs` と `reactor/tcp/unix.rs` の `TcpListener` に `is_unix: bool` を追加し、
  `unsafe fn from_raw_fd_unix(fd) -> Self` を追加する。
- accept 時、`is_unix` なら `getpeername` 由来の `sockaddr_un` を `SocketAddr` へ変換できないため、
  **プレースホルダ `127.0.0.1:0`** を peer_addr として返す（ホットパスに分岐 1 個のみ）。
  IP ブロックリスト・アクセスログはこの値を見る（README に明記）。
- `reactor/tcp/windows.rs` は変更しない（B-69 の教訓に従い、そもそも UDS を cfg で提供しない）。
- `TcpStream` 側は無変更（`set_nodelay` は `let _ =` で無視される既存呼び出しのみ）。

### 既知の制約（ドキュメント化）

- FreeBSD の `sendfile(2)` 静的配信高速化（F-155）は AF_UNIX ソケットでは使えない場合がある。
  失敗時は通常の write 経路にフォールバックする。
- Landlock / capsicum / OpenBSD unveil を使う場合、ソケットファイルを作るディレクトリへの
  書き込み権が必要。

## テスト

- 単体: `ListenAddr::parse` の網羅（`unix:` / `host:port` / 不正）、Windows での拒否。
- 統合/E2E: UDS で listen し、`curl --unix-socket` 相当（テストクライアントから直接 connect）で
  HTTPS / h2c が動作すること。stale socket の再起動が成功すること。
