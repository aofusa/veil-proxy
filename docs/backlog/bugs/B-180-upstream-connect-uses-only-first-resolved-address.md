# B-180: 上流・HTTP クライアントのホスト名の接続が、解決した先頭のアドレスしか試さない（macOS の `localhost` で拒否）

**状態: 修正済み（v0.10.0）**

## 出典

F-276 の macOS（`ssh llm`）の `tests/api/library_e2e.sh all` の kit 節で、`veil::api::client` から自分の TLS リスナー
（`127.0.0.1:<port>`）へ `https://localhost:<port>/...` で要求すると `Connect: connect to localhost:<port> failed: Connection refused`
になった（`client HTTPS h2 via ALPN`・`client HTTPS HTTP/1.1`・`grpc client TLS` の 3 件。2026-10-26）。

## 原因

`TcpStream::connect_str`（io_uring・reactor の unix/windows の 3 実装）は `runtime::dns::resolve` で解決した **先頭のアドレスだけ**
へ接続していた。macOS の `/etc/hosts` は `localhost` を `::1` → `127.0.0.1` の順に返すので、IPv4 だけで待ち受ける上流には
つながらない。Linux は `::1` が `ip6-localhost` なので表に出ていなかった（B-113 のテストの注記も「先頭のアドレスへ接続する」前提）。
プロキシの上流（`url = "http://localhost:..."`）・ヘルスチェック・`veil::api::client`・gRPC の `Channel` も同じ経路。

## 修正

- `runtime::dns::connect_each(addr, connect)`: 解決したアドレス（重複を除いた全部。ワーカーごとのキャッシュに `Arc<[SocketAddr]>`
  で持つので、キャッシュに当たったときは `Arc` の複製だけで確保しない）を順に試し、最初に成功したものを返す。全部だめなら最後のエラー。
  先頭以外でつながったら、そのアドレスをキャッシュの先頭へ移す（次からは最初に試す。並べ替えはその時だけ）。
- 3 つの `connect_str` をこれに置き換えた。待ち時間の上限は従来どおり呼び出し側の `timeout` が全体に掛かる（Happy Eyeballs はしない）。
- FreeBSD の capability mode 下の接続ブローカー（F-182）は従来どおり先頭のアドレス（許可リストの解決はブローカー側）。

## 試験

- 単体: `runtime::dns::tests::connect_each_falls_back_and_prefers_the_working_address`（先頭が拒否されたら次へ・成功したアドレスが
  先頭に移る・全部だめなら最後のエラー）。
- macOS の `library_e2e.sh kit` で上の 3 件が通る。
