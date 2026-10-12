# F-200: 待ち受け設定の整理（`listen` / `listen_cleartext`）

**状態: 進行中（feat/f197-f200-impl）。後方互換性は持たない（旧キーは削除）**

## 要望

ポートの待ち受け設定が複数のキーに分かれて複雑になっているので整理する。

- `listen` で **TLS** の HTTP/1.1・HTTP/2・HTTP/3 を受ける。
- `listen_cleartext` で **平文**の HTTP/1.1 と h2c を受ける。
  - `allow_http` / `allow_h2c` で平文 HTTP/1.1・h2c をそれぞれ受けるかを設定する（既定は両方 `true`）。
  - 今の HTTPS への強制リダイレクト（`[server].http`）は `enable_secure_redirect` に変え、既定は `true`。
- `[http3] enabled = true` なら `listen` と同じアドレスで HTTP/3 を起動する。`[http3] listen` を書けばそちらで起動する。
- 後方互換性は不要。旧キーは削除して整理する。

## 現状（調査結果）

待ち受けに関わるキーが 8 個あり、組み合わせで挙動が変わる:

| 旧キー | 意味 |
|--------|------|
| `[server].listen` | TLS のメインリスナー。`tls_only = false` かつ `h2c_enabled = true` なら同じポートで平文も検出（MSG_PEEK） |
| `[server].http` | HTTP → HTTPS の 301 だけを返す平文リスナー（`listen_http_addr`、`HTTPS_REDIRECT_PORT`） |
| `[server].tls_only` | メインリスナーで平文を拒否（既定 `true`、F-163） |
| `[server].h2c_enabled` / `h2c_listen` | h2c を有効化 / 専用リスナー。`h2c_listen` 未指定（または listen と同じ）だと **H2C 専用サーバ**になり TLS リスナーが起動しない |
| `[server].h2c_http1` | H2C 専用リスナーで平文 HTTP/1.1 も受ける（F-188 で追加） |
| `[server].http2_enabled` | TLS の ALPN で h2 を選べるか |
| `[server].http3_enabled` + `[http3].listen` | HTTP/3 の有効化と UDP アドレス（未指定なら listen と同じ） |

参照箇所は `src/`（config・entry・proxy・server・pool・listen_addr・api）、`tests/`（e2e_setup.sh の生成設定・e2e・統合・
standalone）、`tools/perf/`（`gen_configs.sh` と生成済み 30 構成・run_perf・alloc_ab・leak_check・BSD 版）、
`tools/container_security/`、`docker/assets`・`contrib/config`・`deploy/kubernetes`・`examples/`、ガイド（英日）で計 70 ファイル以上。

## 新しい設定

```toml
[server]
listen = "0.0.0.0:443"            # TLS: HTTP/1.1 + h2（ALPN）。省略可（平文だけのサーバ）
listen_cleartext = "0.0.0.0:80"   # 平文: HTTP/1.1 + h2c（prior knowledge）。省略可
allow_http = true                 # listen_cleartext で平文 HTTP/1.1 を受ける（既定 true）
allow_h2c = true                  # listen_cleartext で h2c を受ける（既定 true）
enable_secure_redirect = true     # listen_cleartext の要求を https://<host>[:<listen のポート>] へ 301（既定 true）
http2_enabled = true              # listen の ALPN で h2 を選べるか（現行どおり）

[http3]
enabled = false                   # true なら listen と同じアドレス（UDP）で HTTP/3
# listen = "0.0.0.0:8443"         # 書けばこちらで起動
```

### 規則

| 項目 | 決定 |
|------|------|
| リスナーの数 | `listen`・`listen_cleartext` の少なくとも一方が必要。両方同じアドレスはエラー（1 ポートでの TLS/平文の自動判別はやめる） |
| `listen` | TLS 専用（`[tls]` 必須）。平文が来たらハンドシェイク失敗で閉じる（旧 `tls_only = true` 相当が唯一の挙動） |
| `listen_cleartext` | プロトコル検出（MSG_PEEK）で HTTP/1.1 か h2c かを判別し、`allow_http`・`allow_h2c` で受けるかを決める。拒否は HTTP/1.1 なら 426 / h2c なら GOAWAY(PROTOCOL_ERROR)（B-102 の作法で読み捨ててから閉じる）。両方 false はエラー |
| `enable_secure_redirect` | `true` なら `listen_cleartext` に来た要求（HTTP/1.1・h2c とも）を `https://<Host>:<listen のポート>` へ 301。`listen` が無いときに `true` は設定エラー（リダイレクト先が無い）。平文で配信したいときは `false` |
| HTTP/3 | `[http3] enabled = true` で起動。`[http3] listen` 未指定なら `listen` のアドレス（UDP）。`listen` が無いのに `enabled = true` はエラー（QUIC は TLS 必須）。`Alt-Svc` は HTTP/3 の実際のポートで広告 |
| UDS | `listen`・`listen_cleartext` とも `unix:<path>` を受ける（F-164 の規則のまま）。HTTP/3 は UDS 不可 |
| `[tls]` | `listen` があるときだけ必須（F-188 の「平文専用構成なら省略可」はこの規則に置き換わる） |
| 削除するキー | `[server].http`・`tls_only`・`h2c_enabled`・`h2c_listen`・`h2c_http1`・`http3_enabled`。書かれていたら「削除されたキー（移行先: …）」の設定エラーにする（黙って無視しない） |

### 旧キーからの対応（ガイドに移行表として載せる）

| 旧 | 新 |
|----|----|
| `listen` + `tls_only = true`（既定） | `listen` |
| `http = "0.0.0.0:80"` | `listen_cleartext = "0.0.0.0:80"`（`enable_secure_redirect = true` が既定） |
| `h2c_enabled = true` + `h2c_listen = ":8080"` | `listen_cleartext = ":8080"` + `enable_secure_redirect = false` |
| `h2c_enabled = true`（H2C 専用）+ `h2c_http1 = true` | `listen_cleartext = ...`（`listen` 無し）+ `enable_secure_redirect = false` |
| `tls_only = false` + `h2c_enabled = true`（同一ポートで自動判別） | `listen` と `listen_cleartext` を別ポートに分ける |
| `http3_enabled = true` + `[http3] listen` | `[http3] enabled = true`（+ 必要なら `[http3] listen`） |

## 実装方針

- `config.rs`: `ServerConfigSection` のキーを差し替え、`LoadedConfig`・`RuntimeConfig`・`LoadedConfigWithoutTls` の
  `tls_only`・`h2c_*`・`listen_http_addr` を `listen: Option<ListenAddr>`・`listen_cleartext: Option<ListenAddr>`・
  `cleartext: CleartextPolicy { allow_http, allow_h2c, secure_redirect }` にまとめる。`validate_config` に上の規則。
  削除キーは `serde` の未知キーとして来るので、`Config` に `#[serde(flatten)]` の残余マップを置いて旧キー名を検出する。
- `entry.rs`: リスナーの起動を「TLS ワーカー（listen）」「平文ワーカー（listen_cleartext）」「HTTP/3 ワーカー」の
  3 種類に整理する。`is_h2c_only_server` と HTTPS リダイレクト専用サーバ（`listen_http_addr`）は消える（平文ワーカーの
  `secure_redirect` に統合）。FreeBSD capsicum の bind 完了バリアの数え方を新しいリスナー構成に合わせる。
- `proxy.rs`: `handle_connection`（TLS）から MSG_PEEK の平文判別を外す（TLS リスナーは常にハンドシェイクへ。旧
  `tls_only = true` の高速経路だけが残る＝ホットパスは不変か軽くなる）。平文ワーカーは `detect_protocol_with_buffer` →
  HTTP/1.1（`handle_plain_http1_connection`）/ h2c（`handle_h2c_connection`）/ リダイレクト、の 1 本にする。
- リダイレクトは現行の `handle_http_redirect`（`config.rs` の HTTP → HTTPS 301）を HTTP/1.1・h2c の両方に使えるようにする
  （h2c は HTTP/2 の 301 応答）。リダイレクト先ポートは `listen` のポート（443 なら省略）。
- API ライブラリ（F-188）: 既定設定を `listen_cleartext = "0.0.0.0:8080"`・`enable_secure_redirect = false`・`listen` 無しに。
  `ConfigBuilder::tls()` は `listen` を設定するだけ、`h2c_enabled()`・`tls_only()` は削除し `listen_cleartext()`・`allow_http()`・
  `allow_h2c()`・`secure_redirect()` に置き換える。C・Python の設定 API も同様。
- 生成・テスト資材: `tools/perf/gen_configs.sh` を新キーで生成し直し、構成名（`h2_0_...`）は変えない。
  `tests/e2e_setup.sh` の生成設定、`docker/assets`・`contrib/config`・`deploy/kubernetes`・`tools/container_security`・
  `examples/config.toml` を新キーへ。

## 試験

- 単体: 新しい検証規則（リスナー無し・同一アドレス・`[tls]` 不足・redirect に listen 無し・HTTP/3 に listen 無し・削除キー）。
- standalone E2E: listen のみ / listen_cleartext のみ / 両方、`allow_http`・`allow_h2c` の 4 通り、redirect の有無、HTTP/3 の
  同一アドレス・別アドレス、UDS。
- 既存 E2E（io_uring・epoll）・`tests/api/library_e2e.sh`・kind（musl/glibc × plain/Istio/Linkerd）を新しい設定で全通過。
- 性能: TLS リスナーの MSG_PEEK 分岐が消えるだけなので退行しないはずだが、交互 A/B（3B・54KB、両バックエンド）で確認。

## 受け入れ条件

- 旧キーがソース・設定例・ガイドから消え、`examples/config.toml`・`docs/guide/configuration.md`（英日）に新しい表と移行表がある。
- 旧キーを書いた設定は移行先を示すエラーで起動しない。

## 結果（設定資材・テスト・ガイドの移行）

旧キー（`[server].http`・`tls_only`・`h2c_enabled`・`h2c_listen`・`h2c_http1`・`http3_enabled`）の利用箇所を新キーへ移行した。

- テスト: `tests/e2e_setup.sh` の生成設定（H2C バックエンドは `listen`（ALPN h2 の TLS）+ `listen_cleartext`、UDS の h2c バックエンドは
  `listen_cleartext = unix:...`、プロキシは `listen_cleartext` = h2c ポート（8081）+ `enable_secure_redirect = false` + `[http3] enabled`）。
  **301 リダイレクトの検証は 1 つの平文ポートで h2c 配信と両立できない**ため、別インスタンス `proxy_redirect.toml`
  （`listen` 8450 + `listen_cleartext` 8080、リダイレクト既定）に分けた（`test_redirect_301` は 8080 のまま）。
  `tests/integration_tests.rs`（graceful shutdown の平文接続）と `tests/standalone_e2e.rs`（F-179 は `listen_cleartext` の上書き、
  F-188 は `listen_cleartext` 版に書き換え。`allow_http = false` → 426 + h2c 可、`allow_h2c = false` → h2c 拒否 + HTTP/1.1 可、
  `enable_secure_redirect` の HTTP/1.1・h2c 双方の 301（`Location`）、削除キーの `veil -t` エラーと移行先、検証規則）。
  ハーネスに `h2c_get_with_headers` を追加。
- 設定: `examples/config.toml`（待ち受けの表・移行表）・`contrib/config`・`docker/assets/conf.d`・`deploy/kubernetes/base`
  （`listen` 8443 TLS + `listen_cleartext` 8080、リダイレクト無し）・`tools/container_security`（フィクスチャ・差分/カオス用の生成設定）。
- 計測: `tools/perf/gen_configs.sh` と生成設定（構成名は不変。h2c 構成は `:80` のリダイレクトリスナーが無くなり `listen_cleartext` が h2c ポート）、
  `*_alloc.toml`、`alloc_ab.sh`・`leak_check.sh`・BSD 版（`h1_file_plain` は TLS ポートでの平文受理が無くなったので平文ポートへ）。
- ライブラリ: C は `veil_config_listen`（TLS）・`veil_config_listen_cleartext`・`veil_config_tls`、Python は `Config.listen`（TLS）・
  `listen_cleartext`・`tls`、`run(listen_cleartext=, listen=, tls_cert=, tls_key=)`、サンプルの `VEIL_LISTEN` は平文へ。
- ガイド（英日）: configuration（リスナー表・検証規則・移行表）・protocols（英に H2C 節を追加）・library・kubernetes・running・acme・testing、
  AGENTS.md の平文計測ルール。
- 試験: lib 1049・integration 60・standalone_e2e 12・E2E（io_uring / epoll とも 565）・`tests/api/library_e2e.sh` の rust / c / python が通過。
  kind（k8s）と `examples/container/e2e.sh`（docker イメージが要る）は未実施。
