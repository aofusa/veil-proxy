# B-249: xDS RBAC の変換で、未対応の permission が DENY でも「一致しない」になり、`Principal::url_path` が「全員」になる（fail-open）

**状態: 未修正（2026-10-27 起票）**

## 出典

2026-10-27 のセキュリティレビュー 2 回目の MS-01（`docs/artifacts/security/analysis/07-f281-followup.md`）。

## 事象

`src/mesh/xds/rbac.rs` のモジュール先頭の約束は「変換できない規則は ALLOW なら一致させず、DENY なら一致させる」。これは CEL の条件
（`unsupported_condition`、L267〜L275）にしか実装されていない。

- `perm`（L336）: `Permission::Metadata`・`Matcher`・`None` を action に関わらず `Not(Any)`（一致しない）にする。**DENY ポリシー** の permission が
  これらだと、そのポリシーは何も拒否しない。Istio の `AuthorizationPolicy` の `when` 条件は Envoy の `metadata` や `matcher` に変換されることがある。
- `princ`（L356）: `Principal::UrlPath` を **`Any`** にする。ALLOW ポリシーの「このパスの principal だけ」が **全員** になる。
  （Envoy では principal の `url_path` は permission と同じ意味で使われる。）

## 影響

- P2（メッシュ機能のみ）。Istio の RBAC が意図より緩くなる。シャドウモードのログでは気づかない。

## 改修案

- `perm`・`princ` に action を渡し、未対応の規則は ALLOW → `Not(Any)`、DENY → `Any` にする（CEL の条件と同じ）。
  `Principal::UrlPath` は `Permission::UrlPath` と同じ `Path` マッチに変換する。
- 変換時に `warn!` で「未対応の規則を安全側に倒した」ことをポリシー名付きで出す（起動時・更新時のみ）。
- 単体テスト: `Permission::Metadata` を含む DENY が拒否になる、`Principal::UrlPath` を含む ALLOW が全員を通さない。

## 関連

- [F-220](../features/F-220-mesh-authorization.md)、[F-282](../features/F-282-security-review-followup-hardening.md)
