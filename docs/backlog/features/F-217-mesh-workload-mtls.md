# F-217: ワークロード ID の mTLS（受け・送り・ID の検証・証明書の差し替え・PERMISSIVE）

**状態: 完了（feat/mesh-dataplane-impl、2026-10-15）**

## 現状

- 受け側（`[tls]`）はサーバ証明書だけ。**クライアント証明書の要求・検証は無い**（`with_no_client_auth`）。
- 上流の TLS は検証の有無（`verify_cert`）だけで、独自の CA・**クライアント証明書の提示**・期待する相手の ID の指定は無い。
- 証明書のホットリロードはファイル（F-105・F-180）と ACME。制御プレーンから渡される証明書の経路は無い。
- 制御プレーン向け（`ctrl_http`）だけはクライアント証明書を出せる。

## 設計

- **ID の型**: `WorkloadId`（Istio は SPIFFE ID = URI SAN `spiffe://<trust-domain>/ns/<ns>/sa/<sa>`、Linkerd は DNS SAN
  `<sa>.<ns>.serviceaccount.identity.linkerd.<trust-domain>`）。検証済みの相手の ID は接続の状態に 1 回だけ持つ
  （要求ごとに証明書を解析しない）。
- **受け側**: rustls の `ClientCertVerifier` を実装し、信頼アンカー（メッシュの CA）で検証、相手の ID を取り出す。
  Istio の PERMISSIVE は F-216 の検出で「TLS かつメッシュの ALPN」なら mTLS、平文ならそのまま（同じポート）。
  STRICT は平文を拒否。Linkerd は mTLS でない接続も受けて ID 無しとして扱い、認可（F-220）で判断する。
- **送り側**: エンドポイントごとの期待する ID（Istio は DestinationRule の `ISTIO_MUTUAL` と EDS の `subjectAltNames`、
  Linkerd は Destination の `TlsIdentity`）を `ServerCertVerifier` で検証し、自分の証明書を提示する。ALPN は相手のメッシュの
  取り決め（Istio は `istio-peer-exchange` 等、Linkerd は HTTP/2 か `transport.l5d.io/v1`）。上流 HTTP/2 は今の
  `upstream_mux` を使い、プールのキーに期待する ID を含める（ID が違うエンドポイントの接続を共有しない）。
- **証明書の差し替え**: 自分の証明書・鍵・信頼アンカーは `ArcSwap<Arc<CertifiedKey>>` と検証器の `ArcSwap`。制御プレーン
  （Istio は SDS、Linkerd は Identity の Certify）から専用スレッドで受け取り、差し替えるだけ。既存の接続はそのまま、新しい
  ハンドシェイクから新しい証明書。鍵はメモリのみ・zeroize（F-105 と同じ）。有効期限の前に更新が来なければメトリクスと
  ログで警告する。
- **kTLS**: 終端後の kTLS へのオフロードはそのまま使う（mTLS かどうかは関係ない）。
- **HTTP/3**: メッシュの Pod 間は TCP だけなので対象外。

## 受け入れ条件

- （必須、F-215）既存のプロキシの性能が退行しない（`tools/perf/suite_ab.sh` の main との交互 A/B、比の中央値 0.98 以上・0.95 未満なし）。
  コンテナで試すときのイメージは musl 版。

- 単体: SPIFFE / Linkerd の SAN の抽出と照合、期限切れ・信頼アンカー違い・ID 違いの拒否、差し替え後の新規ハンドシェイクが
  新しい証明書を使う。
- 統合: 自前の CA（rcgen）で発行した 2 つのワークロードの証明書で、veil 同士の mTLS・PERMISSIVE の同居・STRICT の拒否。
- 設定（メッシュ無しでも使える一般機能として）: `[tls] client_auth = "require" | "optional"`・`client_ca`、上流の `tls_client_cert`・
  `tls_ca`・`expected_san`。`docs/guide/tls.md`（英日）と `examples/config.toml`。
- 性能: mTLS の有無で既存の TLS の構成（`tools/perf` の TLS 構成）が退行しない（交互 A/B）。


## 実装の結果

- メッシュ: SDS / Linkerd の Identity の証明書でワークロードの mTLS（SPIFFE / Linkerd の DNS の SAN の検証、PERMISSIVE / STRICT、
  証明書の差し替えは次のハンドシェイクから）。
- TOML（メッシュ無しでも使える一般機能）: `[tls] client_auth`・`client_ca`、上流の `tls_client_cert`・`tls_client_key`・
  `tls_ca`・`expected_san`（`[upstreams.<name>]` と Proxy の action）。`src/upstream_tls.rs`（`TlsProfile`）・`src/x509_san.rs`。
  自己完結の E2E `f217_toml_mtls_server_client_auth_and_upstream_client_cert`。`docs/guide/tls.md`（英日）・`examples/config.toml`。

## 関連

F-215、F-216（検出）、F-220（ID を使う認可）、F-222（SDS）、F-223（Identity）、F-105・F-180（証明書のリロード）。
