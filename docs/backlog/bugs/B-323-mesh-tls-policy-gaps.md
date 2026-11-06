# B-323: メッシュの TLS・ポリシーの検査漏れ（XFCC の偽装・上流の名前・失効リスト・Linkerd の inbound）

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F28・F29・F34・F52）で判明。

- **F28**: 受けた `x-forwarded-client-cert` を、相手がクライアント証明書を出していない（平文・証明書なしの TLS）ときも
  `SANITIZE_SET`・`APPEND_FORWARD` で残していた。アプリは偽の XFCC を本物の身元として読める。
- **F29**: 上流への TLS の検証（`MeshServerVerifier`）は `server_name` を見ず、SAN の条件が無ければ信頼アンカーに
  つながる証明書を宛先を問わず通していた（DestinationRule の SIMPLE で subjectAltNames を書かない構成）。
- **F52**: 検証コンテキストの `crl` を型に持たず、制御プレーンが送った失効リストを黙って捨てていた（失効した証明書を通す）。
- **F34**: Linkerd の inbound はポートのポリシーが 3 秒で届かないと、認可せずにアプリへ中継していた（識別子の発行待ち・
  制御プレーンの遅延で常に開く）。

## 修正

- XFCC: 受けた値を残すのは TLS で相手の証明書があるときだけ（`ALWAYS_FORWARD_ONLY` は従来どおり常に残す）。Envoy と同じ。
- 上流の名前: SAN の条件が無く SNI（DNS 名）があれば webpki で名前を検査する。SNI の無い接続は従来どおり。
  `trusted_ca` の無い SAN の条件は拒否（NACK）する。Host からの自動 SNI（`auto_sni`）は未実装。
- 失効リスト: `crl` を型に持ち、含む xDS の TLS コンテキスト・SDS の秘密を拒否（NACK）する（検査は未実装）。
- Linkerd: ポリシーが届かなければ `default_inbound`（`LINKERD2_PROXY_INBOUND_DEFAULT_POLICY`）で判定する。

## 試験

- `mesh::identity::tests::server_verifier_checks_name_without_san_matchers`。
- 文書: `docs/guide/mesh.md`（英日）の違い・制限。
