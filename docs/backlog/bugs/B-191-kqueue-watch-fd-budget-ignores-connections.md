# B-191: kqueue（BSD・macOS）の静的配信の監視が RLIMIT_NOFILE の 1/4 まで fd を握り、接続の受理が上限より前に EMFILE で失敗する

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のデータプレーンのレビュー（`docs/artifacts/v0100/review-dataplane.md` の L-4）。F-272。

## 現象

kqueue の監視はルートの配下のディレクトリと通常ファイルを全部開いて持つ。`ulimit -n 65536`・`max_concurrent_connections` を
60000 前後にして数万ファイルの静的ルートで `watch = true` にすると、監視スレッドが最大 16384 本の fd を持ち、接続の受理が
`max_concurrent_connections` に届く前に EMFILE で失敗する。警告は監視の上限についてだけで、接続側の上限が実質下がることは
分からなかった。

## 直したこと

- 監視が持ってよい fd の数を、`RLIMIT_NOFILE` の 1/4 と、`RLIMIT_NOFILE` から接続の分（`[security]
  max_concurrent_connections` の 2 倍 + 1024）を引いた数の小さい方にする（`max_concurrent_connections = 0`＝無制限なら従来どおり
  1/4）。`cache::watch::register` が `max_concurrent_connections` を受け取り、予算は設定のロードのたびに決め直す。
- 予算が変わったら（起動時など）info で「監視するファイル・ディレクトリごとに fd を 1 つ持ち、最大いくつ」を出す。
- 文書: `docs/guide/compression-cache.md`（英日）、`examples/config.toml` の `open_file_cache_watch`。

## 試験

- 単体 `cache::watch::tests::kqueue_fd_budget_leaves_room_for_connections`（予算の計算。Linux でも走る）。kqueue の経路は
  `cargo check --target x86_64-unknown-freebsd`・`x86_64-apple-darwin`・`x86_64-unknown-netbsd` で型検査した。実機・VM での
  確認は他の OS の作業と合わせて行う。
