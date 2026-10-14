# F-220: メッシュの認可・認証（RBAC・JWT・ext_authz）

**状態: 未着手（2026-10-14 起票・設計。親 F-215）**

## 目的

Istio の AuthorizationPolicy（ALLOW・DENY・AUDIT・CUSTOM）・RequestAuthentication（JWT）・PeerAuthentication（F-217）と、
Linkerd の Server・AuthorizationPolicy・MeshTLSAuthentication・NetworkAuthentication を、inbound のポリシーとして実装する。
今の veil の認可はルート単位の IP の許可・拒否とレートリミット（`[route.security]`）だけ。

## 設計

- **ポリシーエンジン**（`src/mesh/authz.rs`）: 規則 = 送信元（主体 = F-217 の相手の ID・名前空間・IP の CIDR・
  JWT の主体 / claim）× 宛先（ポート・ホスト・パス・メソッド）× 条件（ヘッダ・SNI 等）。評価順は Istio と同じ
  CUSTOM → DENY → ALLOW（ALLOW の規則が 1 つでもあれば、どれにも一致しなければ拒否）。Linkerd は「Server の既定ポリシー
  （all-unauthenticated・all-authenticated・cluster-authenticated・deny 等）+ 認可の規則」。
- **評価の段**: L4 で決まる規則（主体・IP・ポート）は接続の確立時に 1 回評価して結果を接続に持つ。L7 の規則（パス・メソッド・
  ヘッダ・JWT）だけを要求ごとに評価する（ロード時に規則をポートとメソッドで索引付けし、要求ごとに走査する規則を減らす）。
  要求ごとのアロケーション無し。
- **拒否の応答**: HTTP は 403（Istio の `RBAC: access denied`）、gRPC は `PERMISSION_DENIED`、L4 は接続を閉じる。
  拒否の理由はメトリクス（ポリシー名つき）と `debug!` のログ（要求ごとに `info!` を出さない）。
- **JWT**: `jsonwebtoken` 系の検証（RS256・ES256・EdDSA）。JWKS は専用スレッドで取得・キャッシュ・定期更新（`jwksUri`）し、
  鍵の集合を `ArcSwap` で差し替える。検証済みの claim は要求のローカルに持ち、認可と、ヘッダへの転記（`outputClaimToHeaders`）に使う。
- **ext_authz（CUSTOM）**: gRPC（`envoy.service.auth.v3.Authorization/Check`）と HTTP の外部認可。要求ごとに外部へ問い合わせる
  ので、上流と同じ非同期の接続プール（`upstream_mux`）を使い、タイムアウト・失敗時の扱い（fail open / close）を持つ。後回しでよい。
- **dry-run**（Istio の `istio.io/dry-run` 注釈・AUDIT）: 判定をメトリクスとログにだけ出す。
- **安全側**: 変換できない規則（未知のフィールド・未対応の条件）を含むポリシーは **ALLOW なら規則ごと無効（＝その分は許さない）、
  DENY なら全体を拒否扱い**にして NACK・警告する（認可の抜けを作らない）。

## 受け入れ条件

- （必須、F-215）既存のプロキシの性能が退行しない（`tools/perf/suite_ab.sh` の main との交互 A/B、比の中央値 0.98 以上・0.95 未満なし）。
  コンテナで試すときのイメージは musl 版。

- 単体: 規則の評価順・既定の拒否・PERMISSIVE で ID が無い接続・JWT（期限・発行者・audience・JWKS の更新）。
- F-225 の kind で Istio の公式タスク（Authorization の HTTP・TCP・JWT・deny・dry-run、Authentication Policy）と Linkerd の
  タスク（Restricting Access To Services・Per-Route Policy）を通す。
- メッシュ無しの TOML からも使えるようにする（`[route.security]` の拡張か別のセクション）。`docs/guide/security.md`（英日）。

## 関連

F-215、F-217（主体の ID）、F-218（Policy の型）、F-222・F-223（制御プレーンからの変換）。
