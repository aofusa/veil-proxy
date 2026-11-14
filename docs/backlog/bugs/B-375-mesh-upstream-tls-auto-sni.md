# B-375: メッシュの上流の TLS が auto_sni・auto_san_validation を読まず、名前を検査せずに繋いでいた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx のセキュリティスキャン（`CLAUDE-SECURITY-20261113-030705` の F7。MEDIUM）で判明。B-323 の残り
（「Host からの自動 SNI は未実装」）。

## 事象

Istio は DestinationRule の TLS（`SIMPLE`・`MUTUAL`）に `sni`・`subjectAltNames` を書かないとき、クラスタの
`HttpProtocolOptions.upstream_http_protocol_options` に `auto_sni`・`auto_san_validation` を付ける（要求の Host を SNI にして、
その名前で証明書を検査する）。veil はこれを読まず、SNI が空なので宛先の IP を名前にして繋ぎ、SAN の条件も無いため
信頼アンカー（システムの CA のこともある）につながる証明書なら宛先を問わず通していた。外向きの TLS の発信
（ServiceEntry + DestinationRule SIMPLE）を経路上で横取りする相手に、資格情報を含む要求を渡し得る。

## 対応

- `UpstreamHttpProtocolOptions`（auto_sni・auto_san_validation）を読み、`ClusterCfg::auto_sni` にする（どちらかがあれば）。
- HTTP の要求（HTTP/1.1・HTTP/2・ミラー）は `Picked::with_request_host` で上流へ送る Host（ポートを除く）を TLS の名前にする。
  接続のプールの鍵に名前を入れる（別の名前で検査した接続を使い回さない）。サーバ名にならない Host は繋がない。
- そのクラスタの HTTP の接続は IP の名前でも証明書を検査する（`MeshServerVerifier::verify_ip_name`）。TCP・SAN の条件の
  ある接続・Istio の mTLS（下線を含む SNI）は従来どおり。`override_auto_sni_header` は未対応（`Host`・`:authority` を使う）。

## テスト

- `mesh::xds::translate::tests::cluster_reads_auto_sni`、`mesh::upstream::tests::request_host_becomes_tls_server_name`、
  `mesh::identity::tests::server_verifier_checks_name_without_san_matchers`（IP の名前の検査）。
- 文書: `docs/guide/mesh.md`（英日）。
