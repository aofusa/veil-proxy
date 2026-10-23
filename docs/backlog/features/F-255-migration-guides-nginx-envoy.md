# F-255: nginx・Envoy からの移行ガイド

**状態: 実装済み（feat/distribution-tickets、2026-10-23。v0.9.0 で公開）**

## 目的

- nginx・Envoy を使っている人が、今の設定を veil の `config.toml` に置き換えられるようにする。
  - 概念の対応、設定の項目ごとの対応、置き換えた例を示す。
  - veil に無いもの・振る舞いの違いを、隠さずに書く。
- 書いた対応が正しいことを試験で確かめ、veil の設定が変わったら試験が落ちるようにする。

## 今の状態

- 移行の文書は無い。対応の知識は、次のところに散らばっている。
  - nginx: 性能の比較（`docs/perf/`・`tools/perf/`）は、nginx と同じ条件の設定を持っている。
    `open_file_cache` などの対応はそこにある。
  - Envoy・xDS: VDS は Envoy の xDS（State-of-the-World）の小さな部分集合（`docs/guide/vds.md`）。
    メッシュのデータプレーンの置き換え（`mesh.md`、F-215）の文書もある。
  - Proxy-Wasm: ABI 0.2.1 のフィルタを動かせる（`wasm.md`）。Envoy・nginx の Wasm モジュールとの互換の範囲は、文書にまとまっていない。
- **設定の検査:** 文書の中の TOML の例を `veil -t` で検査する仕組みは無い。

## 設計

### 文書

- `docs/guide/migration-nginx.md`・`docs/guide/migration-envoy.md` と、日本語版（`docs/guide/ja/`）を足す。
- `docs/guide/README.md`（英日）の目次と、README（英日）の Documentation にリンクする。
- 各文書の構成:
  1. **概念の対応**の表。
     - nginx: `http`/`server`/`location`/`upstream` ↔ `[server]`・`[[route]]`（`conditions`・`action`）・upstream のグループ。
     - Envoy: listener/filter chain/route configuration/cluster/endpoint ↔ 同じく。
  2. **項目ごとの対応**の表。列は「元の設定」「veil の設定」「違い・注意」「対応なし（代わりの方法）」。
     - 下の一覧を、`src/config/` と `examples/config.toml` に突き合わせて埋める。推測で「対応」と書かない。
  3. **置き換えの例**。元の設定と veil の設定を並べる。
  4. **運用の違い**:
     - リロード（`nginx -s reload`・Envoy の hot restart ↔ SIGHUP）、`nginx -t` ↔ `veil -t`
     - ログの形式、メトリクスの名前（`observability.md`）
     - 終了のしかた（graceful shutdown）
  5. **性能の設定の対応**:
     - `worker_processes` ↔ `threads`、`open_file_cache` ↔ `static_file_cache` + `open_file_cache`（F-157）
     - kTLS、HTTP/3 の `mmsg_batch_size`・`recv_drain_max`
  6. **移したら確かめること**の手順。
- **nginx で埋める項目:**
  - `listen`（ssl・http2・quic）、`server_name`、`location` の一致の順序（前方・正規表現・完全一致）
  - `root`/`alias`/`try_files`/`index`、`return`/`rewrite`
  - `proxy_pass`・`proxy_set_header`・`proxy_http_version`・`proxy_buffering`・タイムアウト
  - `upstream`（重み・`least_conn`・`hash`・`ip_hash`・`max_fails`）
  - TLS（`ssl_protocols`・`ssl_ciphers`・mTLS・OCSP stapling・セッションの再開）
  - `gzip`/`brotli`、`proxy_cache`
  - `limit_req`/`limit_conn`、`allow`/`deny`、`auth_basic`、`add_header`
  - WebSocket、gRPC（`grpc_pass`）、`stream`（L4）
  - `access_log`/`log_format`、`stub_status`
  - Lua（OpenResty）↔ Proxy-Wasm（Lua のフィルタの例。`examples/wasm-filters/lua-filter`）
- **Envoy で埋める項目:**
  - listener・TLS（`DownstreamTlsContext`・SNI）、HTTP connection manager の route の一致（prefix・path・safe_regex・header）
  - cluster（LB の方式・health check・outlier detection・circuit breaker・retry policy）
  - 重み付きの cluster の分割、header の操作、タイムアウト
  - rate limit、ext_authz（対応の有無と、代わりの方法）
  - Wasm のフィルタ（Proxy-Wasm ABI の互換の範囲、使える host 関数）
  - access log、stats ↔ Prometheus のメトリクス
  - xDS ↔ VDS: 対応する resource の種類、ADS・Delta xDS の有無、control plane の移し方
  - メッシュのサイドカー（`mesh.md`）

### 試験

- **文書の中の設定の検査:** `tools/lint/check-guide-configs.sh` を足し、CI で回す。
  - 移行ガイドの中の ```` ```toml ```` の例をすべて取り出し、`veil -t` を通す。
  - 例をわざと不完全に書くときは、印（`<!-- not-a-full-config -->`）を付けたものだけを除く。
- **等価性の試験:** `tests/migration/` に、代表的な例の組を置く。
  - 組の例: 静的配信、リバースプロキシ + LB、TLS + HTTP/2、WebSocket、gRPC、リダイレクト、header の操作、rate limit。
  - 各組の中身: nginx の設定・Envoy の設定・veil の設定。
  - Docker で 3 つを同じ上流の前に立て、同じ要求の列を送る。
  - 比べるもの: 状態コード・主要な header・本文。
  - 違いが出るのが正しいもの（veil が付けない header など）は、組ごとに許す一覧に書く。
  - nightly で回し、nginx・Envoy のイメージは digest で固定する。
- これらの試験で見つかった veil の不具合・足りない機能は、バグ・機能のチケットにする。

## 受け入れ条件

- 英日の 2 つのガイドが目次から辿れる。項目の一覧のすべての行に、対応・違い・代わりの方法のどれかが書いてある。
- ガイドの中のすべての完全な設定例が `veil -t` を通る（CI）。
- 等価性の試験の組がすべて通る（許す一覧の差を除く）。

## 依存・リスク

- 依存は無い。F-247 の後なら、「入れ方」の節を各ガイドから入手経路へリンクできる。
- nginx・Envoy の版で振る舞いが変わるので、比べた版をガイドに書く。
- 設定の変換ツール（nginx.conf → config.toml）は範囲外。要望があれば別のチケットにする。


## 実装の結果

- 英日の 2 つのガイド。完全な設定例 22 個を CI で `veil -t` に通す（`tools/lint/check-guide-configs.py`）。
- `tests/migration/run.sh`（nightly）: nginx の 3 つ・Envoy の 1 つの組で 17 の要求が同じ応答になる。
- 書く途中で確かめたこと: `path` の `*` 無しは完全一致（`"/"` だけは全部）、プロキシのルートは gRPC-Web を変換しない、
  プロキシのキャッシュは HTTP/1.1 のクライアントと平文の上流だけ（F-257）。

## 関連

- [F-215](F-215-service-mesh-dataplane.md)
- `docs/guide/vds.md`・`wasm.md`・`mesh.md`
- `tools/perf/`（nginx と同じ条件の設定）
