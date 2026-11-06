# B-321: 応答のキャッシュ・静的配信・退避ファイルが要求・ルート・利用者をまたぐ

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F22・F23・F24・F41・F44・F50・F58）で判明。

- **F22**: 応答のキャッシュは `no-store`・`private` だけを保存しない理由にし、`no-cache` の応答を既定の TTL で保存して
  再検証せずに返していた。Cache-Control は 1 行目しか読まず、Cache-Control の無い応答の `Pragma`・`Expires` を
  見ていなかった。利用者の Cookie の付いた要求への応答も共有していた。
- **F23**: キャッシュのキーはメソッド・Host・パス・`key_headers` だけで、全ルートが 1 つのキャッシュを共有していた。
  source_ip・ヘッダ・クエリの条件で一部の相手だけが届くルートの応答を、同じホスト・パスの別のルートの要求へ返した。
- **F41**: キーの `key_headers` の部分を `name:value` を `;` で区切って作っていたので、値に `;x-b:2` を入れると
  別のヘッダの組み合わせのキーになり、その組み合わせの要求へ自分の応答を返させられた。
- **F50**: 静的配信の高速経路はルート自身の dirfd で開く（per-route の封じ込め）が、開いたエントリはパスだけを
  キーに共有のキャッシュへ入れていた。外側のルートが開いた（内側のルートの外を指すシンボリックリンクの）
  エントリを、内側のルートの要求がヒットして配っていた。
- **F24**: HTTP/2・HTTP/3 の静的配信はファイルを丸ごとメモリへ読み、大きさの上限が無かった（大きなファイルへの
  並行の要求でメモリを使い切れる）。
- **F44・F58**: 応答本文のディスクへの退避ファイルの名前が時刻のナノ秒だけで、`File::create`（切り詰め）で開いていた。
  同じ刻みの 2 つの応答が同じファイルを使い、片方の利用者へもう片方の本文を返し得た。

## 修正

- `CachePolicy::check_response`: Cache-Control の行をすべて合わせて読み、`no-cache` は保存しない。Cache-Control が
  無ければ `Pragma: no-cache`・過去の（読めない）`Expires` は保存せず、未来の `Expires` を TTL にする。
  Cookie の付いた要求への応答は `public`・`s-maxage` のときだけ保存する（`key_headers` に Cookie があれば別）。
- キーにルートの名前空間（`CacheConfig::route_ns`。条件・アクション・traffic から設定ロード時に決める）を付け、
  ヘッダは名前と値を長さ付きで書く。
- 高速経路で開いたエントリは、そのルートがパスを含む登録済みルートのうち最も深いもの（`resolve::is_strictest_root`）の
  ときだけ共有のキャッシュへ入れる。
- キャッシュに入らない大きさの丸ごとの読み込みは、プロセス全体で 256 MiB まで（1 つは常に読む）。超えたら 503
  （`StaticFileOutcome::Busy`）。予約は本文の `Bytes` の寿命に結び付ける。
- 退避ファイルのキーはプロセスで一意（pid・起動時刻・連番）にし、`create_new`（O_EXCL）・0600 で開く。

文書: docs/guide/compression-cache.md・security.md・configuration.md（英日）。単体試験
`cache::policy::tests::test_check_response_storage_rules`・`cache::key::tests::test_cache_key_vary_is_injective_and_route_scoped`・
`cache::static_file::tests::uncached_read_budget`・`buffering::handler::tests::test_disk_buffer_roundtrip`。

## 関連

- docs/artifacts/security/v0110-claude-security/（F22・F23・F24・F41・F44・F50・F58）
