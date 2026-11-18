# B-442: メッシュの xDS の検証の設定のうち扱えないもの（ファイルの trusted_ca・ピン留め等）を信頼アンカー無しとして受け、どの証明書も通していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Non-inline trusted_ca (filename/env var) silently disables upstream certificate verification and downstream client-cert auth」から確認。

## 事象

- `mesh::xds::translate::common_tls` はインラインの `trusted_ca` だけを信頼アンカーとして読み、ファイル・環境変数の `trusted_ca` や、ハッシュ・SPKI のピン留めだけの検証コンテキスト（プロトの型に無く読み捨てていた）は信頼アンカー無し（`roots = None`）にしていた。上流の `MeshServerVerifier { roots_name: None }` はどの証明書も通し、下流の `server_config` は `require_client_certificate = true` でも `with_no_client_auth` にしていた。
- EnvoyFilter 等で外部の API への TLS に `trusted_ca.filename` を書くと、経路上の攻撃者の自己署名の証明書で中身を読み書きでき、下流の mTLS 必須のリスナーは証明書の無いクライアントを受けた（Envoy は検証するか設定を拒否する）。

## 対応

- 検査しない検証の設定（インラインでない `trusted_ca`・`verify_certificate_hash`・`verify_certificate_spki`・真の `require_signed_certificate_timestamp`・`system_root_certs`・`ca_certificate_provider_instance`・`watched_directory`・`custom_validator_config`）はプロトの型で読み、CRL（B-323）と同じく拒否（NACK）する（`reject_unsupported_validation`。SDS の秘密も同じ）。信頼アンカーの無い `require_client_certificate` も拒否する（`downstream_tls`・`identity::server_config`）。利用者ガイド（`mesh.md` の英日）に書いた。

## テスト

- `mesh::xds::translate::tests::unsupported_validation_is_rejected`。
