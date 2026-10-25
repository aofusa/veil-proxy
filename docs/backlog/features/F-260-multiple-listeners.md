# F-260: 複数ポートでの待ち受け（リスナーごとのプロトコル・TLS の ALPN・HTTP/3）

**状態: 未着手（2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)）**

## 背景

1 プロセスが持てる待ち受けは `[server] listen`（TLS 1 個）と `[server] listen_cleartext`（平文 1 個）と、`listen` と
同じポートか `[http3] listen` の UDP（HTTP/3）1 個だけ。TLS の ALPN もプロセス全体で 1 つ（`http2_enabled`）。

- 80 と 8080、443 と 8443 のように同じ役割で複数のポートを開けない（nginx の `listen` 複数行・Envoy の複数 listener に当たるもの）。
- 「h2 を出す TLS」と「HTTP/1.1 だけの TLS」、「平文 HTTP/1.1 を https へリダイレクトするポート」と「h2c を受けるポート」
  を同じプロセスに置けない（`docs/guide/configuration.md` の Listeners にも「1 つの `listen_cleartext` ではリダイレクトと
  h2c を両立できない」と書いてある）。
- ベンチマーク（[F-269](F-269-benchmark-submissions.md)）の HttpArena は 8080（平文 HTTP/1.1 + h2c）・8081（TLS、HTTP/1.1 のみ。h2 を
  選んではいけない）・8082（h2c）・8443（TLS h2 + UDP h3）を 1 コンテナで要求する。今は veil を 2〜3 プロセス起動するしかなく、
  コンテナ全体で数える CPU・メモリに待機側のプロセスの固定費が乗る（`docs/artifacts/benchmark/task.md` の 5 章）。

## 調査で分かった今の作り（変更点の地図）

| 項目 | 場所 | 今 |
|---|---|---|
| 設定 | `src/config/mod.rs` `ServerConfigSection`（1253〜1340 行）・`Http3ConfigSection`（981 行） | `listen`・`listen_cleartext` は `Option<String>`。`allow_http`・`allow_h2c`・`enable_secure_redirect`・`http2_enabled`・`unix_socket_permissions`・`threads` は 1 個 |
| 検証 | `src/config/validate.rs` `validate_listeners`（210〜266 行）・`CleartextPolicy`（127〜157 行） | `listen == listen_cleartext` しか比べない。`[[l4]]`・`[acme] challenge_listen` との衝突は見ない |
| リダイレクト先のポート | `src/config/loaded.rs` 275 行 `HTTPS_REDIRECT_PORT`・`src/config/cli.rs` `https_redirect_url` | プロセスに 1 個（`listen` のポート） |
| Alt-Svc | `validate.rs` `apply_alt_svc_from_config`・`pool::build_alt_svc_value` | プロセスに 1 個 |
| TLS の設定 | `tls_server.rs` `load_tls_config`（ALPN は 198 行付近）・`tls_reload.rs` `GLOBAL_TLS_CONFIG` | ハンドシェイクは acceptor の設定でなく大域のスロットを読む（`simple_tls.rs` 881 行・`ktls_rustls.rs` 1486 行・L4 の TLS 終端も） |
| ワーカー | `src/entry.rs` `WorkerListenerSource`（73〜121 行）・TLS 1118〜1305 行・平文 1663〜1946 行・HTTP/3 1538〜1653 行 | リスナーごとに `num_threads` 本のスレッド。接続はリスナーの識別を持たない。平文の判別は大域の `CURRENT_CONFIG.cleartext` を見る |
| CBPF | `src/system.rs` 773 行 `CBPF_ATTACHED`・`server.rs` 871〜893 行 | プロセスで最初に作ったリスナーにだけ付く（平文・L4 は付かない） |
| バリア（B-163） | `entry.rs` 1108〜1111 行 | `num_threads × (TLS の有無 + 平文の有無)` |
| サンドボックスのパス | `src/config/sandbox_paths.rs` `collect_uds_socket_paths` | 2 個のキーだけ見る |
| リロード | `loaded.rs` `LoadedConfigWithoutTls` | 待ち受けの変更を **黙って無視**（→ [B-165](../bugs/B-165-reload-ignores-listener-changes-silently.md)）|
| 終了時の UDS の削除 | `entry.rs` 1987〜1997 行 | 2 個の変数 |
| ライブラリ | `src/api/config.rs` 169〜285 行・`crates/veil-python/src/lib.rs` 126〜136 行・`python/veil/__init__.py` `run`・C API `veil_config_listen*` | 文字列を 1 個ずつ上書き |

調査中に見つけた既存の不具合は別チケットにした: [B-164](../bugs/B-164-reload-http2-disabled-alpn-mismatch.md)（`http2_enabled` を
true → false にリロードすると ALPN は h2 を出し続け、h2 の接続が HTTP/1.1 の処理へ渡る）、[B-165](../bugs/B-165-reload-ignores-listener-changes-silently.md)、
[B-166](../bugs/B-166-cbpf-only-first-listener.md)（CBPF が最初のリスナーにしか付かない）。本チケットはこの 3 件を先に直してから入る
（B-164・B-166 は本設計の部品になる）。

## 設計

### 1. 設定: `[[listener]]`（新設）

`[[route]]`・`[[l4]]` と同じ最上位の表の配列にする。

```toml
[[listener]]
name = "https"                    # 任意。ログ・エラー・redirect_to で使う。省略時は "listener-<添字>"
listen = "0.0.0.0:8443"           # TCP か unix:<path>
tls = true                        # TLS で受ける（[tls] の証明書を使う）
http2 = true                      # ALPN に h2 を出すか（TLS）。既定は [server] http2_enabled
http3 = true                      # 同じポートの UDP で HTTP/3（TLS のみ。TCP のときだけ）
# http3_listen = "0.0.0.0:443"    # UDP を別のアドレスにするとき

[[listener]]
name = "https-h1"
listen = "0.0.0.0:8081"
tls = true
http2 = false                     # ALPN を出さない → HTTP/1.1

[[listener]]
name = "plain"
listen = "0.0.0.0:8080"
tls = false
allow_http = true                 # 平文 HTTP/1.1（既定 true）
allow_h2c = true                  # h2c（既定 true）
secure_redirect = false           # true なら https へ 301（既定 false。[server] の既定と違う点に注意、下の 2.）
# redirect_to = "https"           # リダイレクト先のポートを取る TLS のリスナー名（既定: 最初の TLS のリスナー）

[[listener]]
name = "h2c"
listen = "0.0.0.0:8082"
tls = false
allow_http = false                # h2c だけ
```

- リスナーごとに持てるもの: `listen`・`tls`・`http2`・`http3`・`http3_listen`・`allow_http`・`allow_h2c`・`secure_redirect`・
  `redirect_to`・`unix_socket_permissions`・`threads`（このリスナーのワーカー数。既定は `[server] threads`）。
- プロセスに 1 個のまま（今回変えない）: 証明書（`[tls]`。SNI の複数証明書は既存のとおり）、ルート（全リスナーで同じ `[[route]]`
  を引く。リスナーで分けたいときは `conditions` の host・path で）、`[http2]`・`[http3]` のチューニング、`max_concurrent_connections`
  （プロセス全体）、`[admin]`・`[prometheus]`（全 HTTP のリスナーのパス）。
  - リスナーごとの証明書・リスナーで絞るルート（`conditions.listener = "name"`）・admin を特定のリスナーだけで答える設定は
    後続（要望が出たら別チケット。下の「範囲外」）。

### 2. 既存のキーとの関係（互換）

- `[server] listen`・`listen_cleartext`・`allow_http`・`allow_h2c`・`enable_secure_redirect`・`http2_enabled`・`[http3] enabled/listen` は
  **残す**。設定ロード時に「暗黙のリスナー」へ正規化する: `listen` → `{name = "tls", tls = true, http2 = http2_enabled, http3 = [http3].enabled, http3_listen = [http3].listen}`、
  `listen_cleartext` → `{name = "cleartext", tls = false, allow_http, allow_h2c, secure_redirect = enable_secure_redirect}`。
  以降のコードは正規化したリスナーの列（`LoadedConfig::listeners: Vec<ListenerSpec>`）だけを見る。
- `[server] listen` / `listen_cleartext` と `[[listener]]` を両方書いたときは、正規化した暗黙のリスナーを先頭に足して全部開く
  （片方へ移行中の設定でも動く）。名前の重複はエラー。
- `[[listener]]` の `secure_redirect` の既定は `false`（`[server] enable_secure_redirect` の既定 `true` は F-200 の「平文だけの設定を
  うっかり公開しない」ためのもの。`[[listener]]` は 1 個ずつ明示して書くので、平文のリスナーを書いた人の意図どおり平文で配る）。
  文書に両者の違いを書く。
- `[server] http2_enabled` は TLS のリスナーの `http2` の既定値と、h2c を使えるか（`http2` feature）の既定として残す。

### 3. 検証（`validate_listeners` を作り直す）

- 1 個もリスナーが無い → エラー（今と同じ）。
- **アドレスの重複を全部比べる**: リスナー同士・`http3`/`http3_listen` の UDP 同士・`[[l4]]` の `listen`・`[acme] challenge_listen`。
  `SO_REUSEPORT` で同じポートを 2 回開くとエラーにならず接続が分かれるので、設定で必ず止める（`0.0.0.0:p` と `127.0.0.1:p` も重複とみなす）。
- TLS のリスナーがあるのに `[tls]` が無い → エラー（今と同じ）。`http3 = true` は `tls = true` かつ TCP のときだけ。
- `allow_http = false` かつ `allow_h2c = false` → エラー。`secure_redirect = true` で TLS のリスナーが 1 個も無い → エラー。
  `redirect_to` の名前が無い・TLS でない → エラー。
- `allow_h2c = true` で `http2` feature が無い → エラー（今は起動時のエラー。`-t` で分かるようにする）。
- `client_auth`（F-217）と HTTP/3 の併用の禁止は、`http3 = true` のリスナーがあれば同じく禁止。

### 4. 実行時の構造

**リスナーの文脈 `ListenerCtx`**（リスナーごとに 1 個、`Arc` でワーカーへ渡す。接続ごとの確保は無い）:

```rust
struct ListenerCtx {
    id: u16,                       // 正規化した列の添字
    name: Box<str>,
    tls_profile: Option<AlpnProfile>, // H2AndH1 | H1Only（TLS のとき）
    // 平文の方針はリロードで変わるので、ここには持たず RuntimeConfig の表を id で引く
}
```

- **平文の方針**（`allow_http`・`allow_h2c`・`secure_redirect`・リダイレクト先のポート）: `RuntimeConfig` に
  `cleartext: Box<[CleartextPolicy]>`（リスナーの id で添字を引く）を持つ。接続の判別のたびに `CURRENT_CONFIG.load()` は今も
  呼んでいるので、増えるのは配列の添字 1 回だけ。リスナーの集合が変わらないリロード（方針だけ変える）は今どおり反映する。
- **リダイレクト先のポート**: `HTTPS_REDIRECT_PORT` の大域 1 個をやめ、`CleartextPolicy` に `redirect_port: u16` を持たせる
  （`https_redirect_url` に渡す）。h2c のリダイレクト（`proxy/h2.rs` 1261 行）も同じ。
- **TLS の ALPN**: 大域のスロット `GLOBAL_TLS_CONFIG` を **ALPN のプロファイルごと**（`[Option<Arc<ServerConfig>>; 2]`）に
  する。証明書は共通なので `ServerConfig` は高々 2 個（h2+h1 と h1 のみ）。acceptor は自分のプロファイルの添字を持ち、
  ハンドシェイクでその添字のスロットを読む（今の「大域を読む」性質＝証明書のホットリロードはそのまま）。証明書のリロード
  （`tls_reload`・ACME）は全プロファイルを作り直す。L4 の TLS 終端は今の H1/H2 の扱いのまま（L4 は ALPN を見ない）。
- **接続の振り分け**: `handle_connection` は `CURRENT_CONFIG.http2_enabled` ではなく **ハンドシェイクで決まった ALPN**
  （`tls_stream.is_http2()`）だけで HTTP/2 か HTTP/1.1 かを決める（B-164 の修正そのもの）。
- **ワーカー**: 今と同じく「リスナーごとに `threads` 本」の作りを保つ（暗黙のリスナー 2 個の設定は今と同じスレッド数・同じ
  経路になり、性能が変わらない）。`WorkerListenerSource` をリスナーの列から作り、TLS と平文で 2 か所にある spawn のループを
  「リスナー 1 個を起動する関数」へまとめる（`entry.rs` は配線だけにする。AGENTS.md の行動指針 4）。
  - 1 本のワーカーで複数のリスナーの accept を回す案（スレッド数とアイドルの起床を減らす）は、ここでは採らない。io_uring の
    executor・accept の 1 秒タイムアウト・`alloc_tune::idle_tick` の前提が変わるので、交互 A/B の上で後続のチケットにする。
    ベンチマークで効くのはプロセスを 1 個にすることの方が大きい（プロセスごとのプール・アリーナ・ftlog のチャネル・静的キャッシュの重複が消える）。
- **HTTP/3**: `http3 = true` のリスナーごとに今と同じ HTTP/3 のワーカー群を起動する（スレッドローカルの `H3_SEND_SCRATCH` 等は
  「1 スレッド 1 HTTP/3 サーバ」を前提にしているので、リスナーごとに別スレッドにする）。証明書の PEM を memfd に写すのは
  全 HTTP/3 リスナーの起動の後に消す（今は 100ms 後に消している。列の全員が写し終えたことを数えてから消す）。
- **Alt-Svc**: 今の大域 1 個を、`http3 = true` の最初の TLS のリスナーの UDP ポートから作る（複数の HTTP/3 ポートを広告する
  必要は無い。同じ証明書・同じルートなので、どれか 1 個で足りる）。リスナーごとの Alt-Svc は範囲外。
- **CBPF（B-76）**: B-166 の修正で「`SO_REUSEPORT` のグループ（アドレス）ごとに最初の 1 本で付ける」にする。
- **バリア（B-163）**: 期待値を「TCP のリスナーごとの `threads` の和」にする（HTTP/3 は今と同じく数えない）。
- **サンドボックス**: `collect_uds_socket_paths` は全リスナーの `unix:` を集める。capsicum の `CapModeFacts` は HTTP/3 の
  リスナーが 1 個でもあれば今の `http3` と同じ扱い。
- **終了**: UDS のパスを列で持って全部消す。
- **ログ**: 起動時に 1 リスナー 1 行（`listener "https" 0.0.0.0:8443 tls alpn=h2,http/1.1 http3=0.0.0.0:8443 workers=N`）。
  要求ごとのログ・メトリクスにリスナーの名前は足さない（アクセスログの `listener` 項目は要望が出たら後続）。

### 5. リロード

- リスナーの集合（名前・アドレス・`tls`・`http2`・`http3`・`threads`）が変わったリロードは、**警告して古い待ち受けのまま**
  にする（今は黙って無視。B-165 で警告にする）。開き直し（mesh の `desired_listeners` のような動的な追加・削除）は範囲外。
- 平文の方針（`allow_http`・`allow_h2c`・`secure_redirect`・`redirect_to`）はリロードで反映する（今の `cleartext` と同じ）。

### 6. ライブラリ（Rust・Python・C）

- Rust: `App::builder().listener(Listener::tls("0.0.0.0:8443").http3(true))`・`Listener::cleartext("0.0.0.0:8080").allow_http(true)`。
  中身は `config_override` の合成へ `listener` の配列を足す（`deep_merge` は配列を丸ごと置き換えるので、ビルダの中で配列を作ってから
  1 回で設定する。`set("listener.N....")` は既存の添字しか書けない制約は文書に書く）。既存の `listen`・`listen_cleartext`・`http3` はそのまま。
- Python: `veil.run(listeners=[veil.Listener(listen="0.0.0.0:8443", tls=True, http3=True), ...])`（辞書でも可）。`PyConfig.add_listener`。
  既存の `listen=`・`listen_cleartext=` はそのまま。`run` に `http3=` が無いのも足す。
- C: `veil_config_add_listener(cfg, const veil_listener_t *)`。
- ライブラリの既定（`src/api/default_config.toml` の `listen_cleartext = "127.0.0.1:8080"`）は、`listener` を 1 個でも指定したら使わない
  （指定した人が 8080 を余計に開かないように。`listen_cleartext` を明示した人には今どおり）。

## 試験

- 単体（`src/config/`）: 正規化（暗黙のリスナー・名前・既定値）、重複アドレス（`0.0.0.0` と個別アドレス・UDP・L4・ACME）、
  `redirect_to`、`secure_redirect` の既定の違い、`http3` の条件、既存の F-200 の試験がすべて通る。
- E2E（io_uring と epoll の両方。`tests/e2e_setup.sh`）: HttpArena と同じ 4 ポートの構成を 1 プロセスで立て、
  8443 で ALPN h2・HTTP/3、8081 で h2 を選ばない（`curl --http2` でも HTTP/1.1）、8080 で HTTP/1.1 と h2c、8082 で h2c だけ
  （HTTP/1.1 は 426）、リダイレクトのリスナーが `redirect_to` のポートへ 301。`ss -ltnup` で各ポートの持ち主が 1 プロセス。
- リロード: 方針の変更が反映される、リスナーの変更は警告されて変わらない（B-165）。証明書のリロードで 2 つの ALPN プロファイルが
  両方新しい証明書を返す。
- BSD・macOS・Windows: バリアの数（B-163 の試験）とサンドボックスの下での起動（`tools/qemu/`、ssh の実機）。Windows・macOS の
  クロスビルド。
- 性能: 暗黙のリスナー 2 個の設定（今の構成）で交互 A/B（3B・54KB、両バックエンド）が変わらないこと。4 リスナー 1 プロセス対
  2 プロセス構成のアイドル時の CPU（60 秒の `cpu.stat` の差分）と RSS を記録（ベンチマークの判断材料）。

## 文書

- `docs/guide/configuration.md`（英日）の Listeners 節: `[[listener]]` の表・例（HttpArena と同じ 4 ポート・80→443 リダイレクト + 443・
  公開用 TLS と社内用 h2c）・既存キーとの関係・`secure_redirect` の既定の違い・リロードで変えられないこと。
- `docs/guide/protocols.md`・`running.md`・`library.md`（英日）・`crates/veil-python/docs/configuration.md`・`examples/config.toml`。
- `examples/config.toml` 750〜757 行の capsicum の古い記述（B-163 で変わった）もここで直す。

## 範囲外（後続の候補）

（2026-10-25 追記: 後続にした理由と、HttpArena で関係する所）

- **リスナーごとの証明書**: HttpArena の任意の `tls_check` バッジは、:9000 に別の証明書（`/certs-tls`）で HTTP/1.1 を出し、
  その証明書を実行中に差し替えて 30 秒以内に新しい証明書を返すこと（再起動なし）を求める。採点には入らないので v0.10.0 では入れない。
  入れるときは `[[listener]] tls_cert_path`・`tls_key_path` と、`tls_reload` の世代をリスナーの証明書ごとに持つ形にする。
- **1 ワーカーで複数リスナーを回す作り**: HttpArena の latency-10k・latency-1m はコンテナ全体の CPU（アイドルの起床を含む）を数える。
  リスナーごとに `threads` 本のスレッドを持つと、4 リスナー × N 本 + HTTP/3 の N 本が accept の 1 秒タイムアウトと `idle_tick` で起きる。
  起床 1 回は小さい費用だが、64 コアの計測機では 1 秒に 300 回余りになる。F-276 の計測で影響が見えたら別チケットにする。

- リスナーごとの証明書、リスナーで絞るルート（`conditions.listener`）、admin・metrics を特定のリスナーだけにする、
  アクセスログ・メトリクスのリスナー名、リロードでのリスナーの追加・削除、1 ワーカーで複数リスナーを回す作り。

## 受け入れ条件

- 上の 4 ポートの構成が 1 プロセスで動き、HttpArena の validate のプロトコルの検査（8081 で h2 を選ばない等）を満たす。
- 既存の設定（`listen`・`listen_cleartext`）は挙動・スレッド数・性能が変わらない。
- 文書（英日）・`examples/config.toml`・`backlog.md` が更新されている。
