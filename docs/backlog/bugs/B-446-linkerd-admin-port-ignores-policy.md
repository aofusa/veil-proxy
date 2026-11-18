# B-446: Linkerd の管理ポート（4191）が inbound のポリシーを見ず、`/metrics`・`/proxy-log-level` を誰にでも返していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Linkerd admin port (4191) ignores inbound authorization policy and serves /metrics to any network peer」から確認。

## 事象

- `mesh::linkerd::conn::handle` は管理ポートへの接続を `admin::serve` へ渡し、そのポートの inbound のポリシーを見て
  いなかった。`serve` は TLS のクライアントの ID を捨て、`route` は `/shutdown`・`/env.json` だけをループバックに
  限っていた。
- 既定のポリシーが `deny`・`all-authenticated` のクラスタ、または 4191 の Server・AuthorizationPolicy で Prometheus
  だけに許したクラスタでも、Pod の IP の 4191 へ届く相手はクライアントの証明書なしで `/metrics`（要求の authority・
  相手の ID（サービスアカウント）・宛先のアドレス・宛先の Pod のラベル）と `/proxy-log-level` を読めた。
  linkerd2-proxy は管理ポートにも inbound のポリシーを掛ける（制限したクラスタで `linkerd viz allow-scrapes` が要る理由）。

## 対応

- `/live`・`/ready` は従来どおり誰にでも答える（kubelet のプローブ）。`/proxy-log-level` は `/shutdown`・`/env.json` と
  同じく Pod の中（ループバック）からだけ受ける（linkerd2-proxy と同じ）。
- それ以外（`/metrics`）は、Pod の外からなら管理ポートの inbound のポリシー（`discovery::inbound`。3 秒で届かなければ
  既定のポリシー）で、相手の IP と mTLS の ID を使って認可する。HTTP の Server は HTTPRoute の認可
  （`l7::route_authorized`、`allow-scrapes` の `/metrics` の HTTPRoute）、opaque・TLS の Server は接続の認可で見る。
  断れば 403。
- 利用者ガイド（`mesh.md` の英日）に書いた。

## テスト

- `mesh::linkerd::admin::admin_access_tests::admin_policy_gates_metrics`（既定のポリシーごとの判定、プローブ・
  ループバックは見ない）・`shutdown_and_env_are_loopback_only`（`/proxy-log-level` を追加）。
