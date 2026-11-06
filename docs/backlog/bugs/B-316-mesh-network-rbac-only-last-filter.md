# B-316: メッシュの network RBAC がチェーンの最後の 1 つしか効かない（Istio の TCP の DENY が無視される）

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F3、HIGH）で判明。Istio は AuthorizationPolicy の動作
（DENY・AUDIT・ALLOW）ごとに `envoy.filters.network.rbac` を別のフィルタにして同じチェーンに並べ、Envoy は全部を
順に適用する。`src/mesh/xds/translate.rs` の `filter_chain` は network RBAC を 1 つの `Option` に入れていたので、
後のフィルタが前のものを上書きし、最後の 1 つだけが効いていた。ALLOW と DENY を併用した TCP のワークロード
（データベースのポート等）で、DENY で拒否したはずの相手（同じ名前空間の別の ServiceAccount、IP の範囲）が接続できた。
DENY と AUDIT の組では最後の AUDIT（LOG）だけが残り、全部が通っていた。HTTP のチェーン（HCM）でも、
network RBAC は最後の 1 つだけが HTTP の RBAC フィルタとして積まれていた。

## 修正

network RBAC を `Vec` に集め、TCP のチェーンでは全部を順に評価して 1 つでも拒否なら閉じる（`TcpProxyCfg.rbac`）。
HCM のチェーンでは全部を HTTP の RBAC フィルタとして積む。

単体試験 `mesh::xds::translate::tests::filter_chain_keeps_every_network_rbac`（`--features full,mesh`）。

## 関連

- docs/artifacts/security/v0110-claude-security/（F3）
