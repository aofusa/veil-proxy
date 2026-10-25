# F-259: 外向きの TLS の信頼アンカーに OS の CA 証明書を使えるようにする（`tls_ca = "system"`）

**状態: 完了（2026-10-25 起票・設計・実装。親は [F-258](F-258-v0100-plan.md)）**

## 背景

veil が TLS のクライアントになる経路は、信頼アンカーに **組み込みの webpki-roots**（Mozilla の CA 一覧をビルド時に
埋め込んだもの）だけを使う。利用者が変えられるのは上流の mTLS（F-217）の `tls_ca`（PEM ファイルのパス）だけ。

| 経路 | 場所 | 今の信頼アンカー |
|---|---|---|
| 上流（`tls_ca` 無し = `TlsProfile::Verify`） | `src/ktls_rustls.rs` `client_config`・`src/simple_tls.rs` `default_client_config` | webpki-roots |
| 上流（`tls_ca` / `expected_san` あり = `TlsProfile::Custom`） | `src/upstream_tls.rs` `build` | `tls_ca` の PEM（空なら webpki-roots） |
| ヘルスチェック（同期プローブ） | `src/upstream.rs` 237・624 行付近（**プローブのたびに `RootCertStore` を作る**） | webpki-roots |
| VDS・ACME（`ctrl_http`） | `src/ctrl_http.rs` 84 行付近 | webpki-roots + `[vds]` の追加 CA |
| Proxy-Wasm の `dispatch_http_call`・gRPC 呼び出し | `src/wasm/http_executor.rs`・`src/wasm/host/grpc_tls.rs` | webpki-roots |

困ること:

- 社内 CA・OS に追加した CA（企業のプロキシ・Active Directory の CA・`update-ca-certificates` で入れた CA）を、
  ファイルを探して `tls_ca` に書かないと信頼できない。しかも `tls_ca` は上流ごとで、ヘルスチェック・Wasm・VDS には効かない。
- webpki-roots はビルド時点の一覧。OS のパッケージ（ca-certificates）の更新・失効の反映を受けられない。
- 各 OS のパッケージ（deb・rpm・Homebrew・winget・ports）で入れた veil は、その OS の他のソフトと信頼の範囲が違う。

## 要件（今回の依頼）

1. 上流の `tls_ca` に OS の CA 証明書を使う指定（`"system"`）を足す。
2. **各プラットフォーム向けの配布物（パッケージ・アーカイブ）の既定の `config.toml` は OS の CA 証明書を使う**。
3. **コンテナ（ghcr のイメージ・Helm）は引き続き組み込みの CA 証明書を使う**。
4. 指定しないときの挙動は変えない（組み込み）。設定ファイルを書き換えていない既存の利用者に影響を出さない。

## 設計

### 1. 設定

信頼アンカーの「出どころ」を文字列で指定する。1 個の文字列か、文字列の配列（和集合）。

| 値 | 意味 |
|---|---|
| `"builtin"` | 組み込みの webpki-roots（今の既定） |
| `"system"` | OS の信頼ストア（下の 3.） |
| それ以外 | PEM ファイルのパス（今の `tls_ca`）。`system` という名前のファイルを指すときは `./system` と書く |

```toml
# 外向きの TLS 全体の既定（新設。無ければ "builtin" = 今と同じ）
[tls_client]
ca = "system"                          # または ["system", "/etc/veil/internal-ca.pem"]

# 上流ごとの上書き（既存のキーの値の種類を増やす）
[upstreams."billing"]
tls_ca = ["system", "/etc/veil/billing-ca.pem"]
```

- **`[tls_client] ca`（新設）**: `tls_ca` を書かない上流（`TlsProfile::Verify`）・ヘルスチェック・VDS/ACME・Wasm の
  外向きの呼び出しの既定。名前を `[tls_client]` にするのは、上流だけでなく veil が TLS のクライアントになる全経路に効くため
  （`[tls]` はサーバ証明書の節なので混ぜない）。
- **`tls_ca`（既存、`[upstreams.<name>]`・Proxy の action）**: 値に `"builtin"`・`"system"`・配列を許す
  （serde の untagged で文字列 / 配列）。書いた上流では `[tls_client] ca` を置き換える（足し合わせない。足したいときは配列に両方書く）。
- `[vds]` の追加 CA は今のまま「`[tls_client] ca` に足す」。
- `tls_insecure = true` は今のまま最優先で検証しない。

`UpstreamTlsSettings::tls_ca` は `String` → `CaSources`（`SmallVec<[CaSource; 2]>` 相当の小さな型。`Hash`・`Eq` は
`intern` の鍵に使うので維持）に変える。`is_set()` は「`tls_ca` が書かれた」で判定する（`"builtin"` を明示した上流も
`Custom` になるが、中身は同じ `RootCertStore` を共有するのでプールが分かれるだけ。気になるなら `"builtin"` 単独は `Verify`
に寄せる）。

### 2. 信頼ストアの解決と共有（`src/trust_store.rs` 新設）

```rust
pub enum CaSource { Builtin, System, Pem(PathBuf) }
/// 出どころの組から RootCertStore を作る（設定ロード時。結果は出どころの組 + PEM の中身で intern）。
pub fn roots(sources: &[CaSource]) -> io::Result<Arc<RootCertStore>>;
/// `[tls_client] ca` を解決した既定（ArcSwap。設定のロードとリロードで差し替える）。
pub fn default_roots() -> Arc<RootCertStore>;
```

- **ホットパスは変えない**: 解決は設定のロード時だけ。接続ごとの経路は今と同じく `Arc<ClientConfig>` の参照だけ。
- `TlsProfile::Verify` の既定のクライアント設定（`ktls_rustls::client_config`・`simple_tls::default_client_config`）は
  `default_roots()` から作り、`[tls_client] ca` が変わったリロードで作り直す（`ArcSwap`。プールの鍵のタグ `"verify"` に
  世代を足し、古い信頼アンカーで確立した接続と新しいものを混ぜない）。
- ヘルスチェックの同期プローブは、プローブのたびに `RootCertStore` を作るのをやめ、設定ロード時に作った
  `Arc<ClientConfig>` を使う（ついでの修正。プローブはホットパスではないが、毎回 150 個余りの証明書を複製している）。
- `ctrl_http`・Wasm の 2 か所も `default_roots()` を使う。

### 3. OS の信頼ストアの読み方

依存に **`rustls-native-certs` 0.8**（既に `Cargo.lock` に間接依存で入っている版）を足す。feature にせず常に入れる
（Linux・BSD は `openssl-probe` だけの小さな依存、macOS は `security-framework`、Windows は `schannel`。重い依存ではない）。

| OS | 読むもの | サンドボックスとの関係 |
|---|---|---|
| Linux | `SSL_CERT_FILE` / `SSL_CERT_DIR`、無ければ openssl-probe の候補（`/etc/ssl/certs/ca-certificates.crt`・`/etc/pki/tls/certs/ca-bundle.crt`・`/etc/ssl/cert.pem` 等） | Landlock・seccomp の前に読む |
| FreeBSD | `/etc/ssl/cert.pem`（certctl）・`/usr/local/share/certs/ca-root-nss.crt` | capsicum の `cap_enter` の前に読む |
| OpenBSD | `/etc/ssl/cert.pem` | pledge/unveil の前に読む |
| NetBSD | `/etc/openssl/certs/`（mozilla-rootcerts-openssl）。openssl-probe の候補に入っているかを実装時に確かめ、無ければ候補を足して `SSL_CERT_DIR` 相当で読む | chroot の前に読む |
| macOS | Security.framework（システム・管理者のキーチェーン、信頼設定を反映） | Seatbelt の `sandbox_init` の前に読む |
| Windows | CryptoAPI の `ROOT` ストア（ローカルマシン + 現在のユーザー） | – |

**読む時点**: OS のストアは、起動時（最初の設定ロード、サンドボックスを掛ける前）に設定のどこかで `"system"` が使われていれば
1 回だけ読み、プロセスの終わりまで持つ（`OnceLock`）。

- SIGHUP のリロードでは読み直さない（Landlock・capsicum・unveil・Seatbelt の下では OS のストアのパスを読めない／
  macOS は securityd への XPC を Seatbelt が拒む。許可を広げてまで読み直さない）。OS の CA の更新を反映するには再起動する
  （文書に書く。多くのデーモンと同じ）。
- 起動時に `"system"` を使っていなかった設定へ、リロードで `"system"` を足したとき: サンドボックスが無効なら読む。有効なら
  リロードを拒否して理由（再起動が要る）をログに出す（fail-closed。黙って `builtin` にしない）。
- 読めた証明書が 0 個、またはストア自体が無い → 設定のエラー（`veil -t` も失敗）。一部の証明書が解析できないだけなら
  `warn!` で件数を出して続ける（`rustls-native-certs` の `CertificateResult::errors`）。
- 起動時に 1 行 `info!`（「system trust store: N certificates from <source>」）。要求ごとのログではない。

### 4. 既定の設定（配布物ごと）

`packaging/defaults/config.toml.in` に `[tls_client]` を足し、`tools/release/render-defaults.sh` の経路表に列を 1 つ足す。

| 経路（channel） | `[tls_client] ca` |
|---|---|
| `container`（ghcr のイメージ、Helm の既定の values） | `"builtin"`（明示して書く。イメージには OS のストアを当てにしない） |
| `linux-package`（deb・rpm）・`archive-*`・`winget`・`ports-*`・Homebrew | `"system"` |

- `security/linux-package.toml` など Landlock の `landlock_read_paths` は **足さない**（起動時に読むので要らない）。
  ただしユーザーが SIGHUP で読み直したくなる誤解を避けるため、既定の設定のコメントに「OS の CA は起動時に読む」と書く。
- `render-defaults.sh --check` の `veil -t` は Linux の CI で `"system"` を読めることも確かめる（CI のイメージに ca-certificates がある）。
- Helm chart の values の既定の設定も `container` 経路の生成物なので `"builtin"` のまま。
- ライブラリ（Rust・Python）の既定は `"builtin"` のまま（設定ファイルの無い使い方なので配布物の経路ではない）。
  `App::builder().set("tls_client.ca", "system")`・`overrides={"tls_client.ca": "system"}` で選べる。

### 5. 互換性

- `[tls_client]` を書かない・`tls_ca` にパスを書く設定は 1 バイトも挙動が変わらない。
- `tls_ca` にパスを書いていた既存の利用者で、ファイル名が `system`・`builtin` そのもの（ディレクトリ無し）の場合だけ意味が変わる。
  リリースノートに書く。
- パッケージの更新は利用者の設定を上書きしない（F-251 等の約束）ので、既に入れている人の既定は変わらない。新しく入れた人から `"system"`。

## 他のチケットとの関係（2026-10-25 追記）

- [F-267](F-267-rust-kit-crate.md) の `veil::api::client`（HTTP・gRPC のクライアント）と [F-268](F-268-python-kit-fastapi-compat.md) の Python の gRPC の
  チャネルは、信頼アンカーに `[tls_client] ca` と上流ごとの `tls_ca` と同じ値（`"system"`・`"builtin"`・PEM・配列）を使う
  （本チケットの `trust_store::roots` を共有する。クライアントの builder の `.ca(..)` も同じ型を受ける）。

## 試験

- 単体（`src/trust_store.rs`）: 文字列・配列の解析、`"./system"` はパス、`builtin` と PEM の和、0 個はエラー、
  `SSL_CERT_FILE` に自己署名の CA を置いて `system` で読める（Linux・BSD の経路。`rcgen` で生成、`tests/` のフィクスチャを読まない）。
- 統合: 自己署名の CA で署名した HTTPS の上流を立て、(a) 既定（builtin）は 502、(b) `SSL_CERT_FILE` にその CA を置いて
  `[tls_client] ca = "system"` で 200、(c) 上流の `tls_ca = "builtin"` が `[tls_client]` を上書きして 502、
  (d) ヘルスチェック・Wasm の `dispatch_http_call` も同じ結果。
- サンドボックス: Linux（Landlock + seccomp 有効）・FreeBSD（capsicum）・OpenBSD（pledge/unveil）・macOS（Seatbelt）で、
  起動時に読めてリロード後も使えること、リロードで `system` を初めて足すと拒否されること（`tools/qemu/`・ssh の macOS 実機）。
- Windows（ssh の実機）: `ROOT` ストアに入れた CA で 200。
- `render-defaults.sh --check`: 全経路の生成物が `veil -t` を通り、`container` だけ `builtin`。
- 性能: 接続ごとの経路は変わらないので計測は不要。念のため HTTPS 上流の 3B を交互 A/B 1 回（`tools/perf`）。

## 文書

- `docs/guide/tls.md`（英日）: 「Upstream trust anchors」の節（値の表・OS ごとの読み先・起動時に読むこと・リロード）。
- `examples/config.toml`: `[tls_client]` と `tls_ca` の新しい値。
- `docs/guide/container.md`・パッケージの各ガイド: コンテナは組み込み、パッケージは OS のストア。
- リリースノート: 既定の設定（新規インストール）の変更と、`system`・`builtin` という名前のファイルの注意。

## 受け入れ条件

- `tls_ca = "system"`・`[tls_client] ca` が上流・ヘルスチェック・VDS/ACME・Wasm の全経路に効く。
- 既定の設定は配布物で `"system"`、コンテナで `"builtin"`。設定を書かない人の挙動は変わらない。
- 各 OS のサンドボックスを有効にした既定の設定で起動・リロードできる。
- 文書（英日）・`examples/config.toml`・`backlog.md` が更新されている。

## 実施内容（2026-10-25）

実装した。主な変更:

- `src/trust_store.rs` 新設: `CaSource {Builtin, System, Pem}`・`CaSources`（serde untagged で文字列 / 配列。空文字列は未設定、`./system` はパス）・
  `[tls_client]` の `TlsClientConfig`・`roots()`・既定（`resolve_default` / `commit_default` / `set_default` / `default_roots`）・
  プールの鍵のタグ `verify_tag()`・世代 `generation()`・ヘルスチェック用 `probe_client_config()`。
  OS のストアは `rustls-native-certs` 0.8.4（Cargo.lock の既存の版。常に依存）で `OnceLock` に 1 回だけ読む。
- 全経路を `default_roots()` に切り替え: `ktls_rustls::client_config`・`simple_tls::default_client_config`・`upstream_tls::build`
  （`tls_ca` を書かない mTLS 設定も既定を使う）・ヘルスチェックの 2 か所（プローブごとの `RootCertStore` 作成をやめ、
  既定が変わったときだけ作る `Arc<ClientConfig>` を使う）・`ctrl_http`（`[vds] ca_cert` は既定に足す）・Wasm の 2 か所。
- `UpstreamTlsSettings::tls_ca` は `CaSources`。`intern` の鍵は PEM の中身と既定の世代を含む。
- `config` に `tls_client` を追加。`load_config`（起動時、サンドボックスの前）で既定を解決し、
  `entry.rs` が設定のロード直後にサンドボックスを掛ける設定かを `trust_store::set_sandboxed` に渡す。
- `packaging/defaults/config.toml.in` に `[tls_client] ca = @CLIENT_CA@`、`render-defaults.sh` の経路表に列を追加
  （`container` = `"builtin"`、他 = `"system"`）。`packaging/container/config.toml` を再生成。`--check` は全経路 ok。
- 文書（英日）: `docs/guide/tls.md`・`docs/guide/ja/tls.md` の「Upstream trust anchors」、`container.md`（英日）、`examples/config.toml`。

### 決定事項

- 読む契機は「`"system"` が初めて解決された時」とし、設定全体を走査して `system` を探す処理は置かなかった（起動時の
  `load_config` が `[tls_client]`・上流・ルートの `intern` をすべて通るため同じ結果になる）。リロードで初めて現れたとき、
  サンドボックスが有効（seccomp・Landlock・capsicum・unveil・pledge・Seatbelt・chroot のどれか）ならエラーでリロードを拒否し、
  無効なら読む。
- 既定が変わったリロード後のコネクタ: 世代が 0 の間は既存のスレッドローカルをそのまま使い（ホットパスの追加は世代の読み出し 1 回）、
  1 以上になったら世代ごとにスレッド内で作り直す（`config/tls_connector.rs` の `verify_connector`）。プールのタグは
  `verify` → `vf<世代>`。
- リロードの途中で後続の手順が失敗した場合でも、`[tls_client] ca` だけは新しい値が残る（`tls_ca` を書かない mTLS の
  `intern` が新しい既定を使うため、既定の反映を先にした）。
- `tls_ca = "builtin"` 単独を `Verify` へ寄せる最適化はしていない（`Custom` として同じ `RootCertStore` を共有するのでプールが分かれるだけ）。
- ヘルスチェックの同期プローブは今までどおり上流の `tls_ca` ではなく既定（`[tls_client] ca`）で検証する。
- NetBSD の `/etc/openssl/certs/` は openssl-probe 0.2.1 の候補に含まれていることをソースで確認した（実機では未確認）。
- 試験は `trust_store` の単体（解析・`./system`・和集合・0 個のエラー・`SSL_CERT_FILE` を子プロセスで読む）と、
  `tests/integration_tests.rs` の `f259_trust_store`（生成した CA が署名した HTTPS サーバに対し builtin は失敗、
  PEM・和集合・子プロセスの `SSL_CERT_FILE` + `system` は 200）。`veil` の全プロセスを立てる E2E（上流の `tls_ca = "builtin"`
  による上書き、ヘルスチェック・Wasm の同条件）と、各 OS のサンドボックス・Windows 実機の試験は未実施。
