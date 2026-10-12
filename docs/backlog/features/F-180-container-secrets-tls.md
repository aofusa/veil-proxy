# F-180: コンテナでの証明書・秘密鍵の受け渡し（F-07 残件）

**状態: 完了（feat/f188-api-library-vds-k8s）**

## 背景

F-07 の「将来拡張」に残っていた項目。証明書ホットリロード自体は F-03 / F-105 / F-136 で実装済みで、
`[tls] auto_reload` により mtime 監視と SIGHUP で差し替えられる。足りないのは運用サンプルとコンテナ上の検証。

## 改修案

- Docker secrets（`/run/secrets/...`）・Kubernetes Secret のボリュームを `cert_path` / `key_path` に合わせた
  compose / マニフェストのサンプルを置く。Kubernetes Secret の更新はシンボリックリンク（`..data`）の
  差し替えで行われるため、`auto_reload` が追従するかを確認し、追従しなければ対応する。
- PEM 本文を環境変数で渡しメモリ上だけで持つ案は、プロセス環境の露出（`/proc/<pid>/environ`）と
  ゼロ化の難しさを評価してから判断する。

## 受け入れ条件

- サンプルの構成でコンテナを起動し TLS ハンドシェイクが成立する。Secret を更新すると新しい証明書で応答する。
- `docs/guide/`（英日）にコンテナでの推奨運用を書く。

## 結果（2026-10-12、feat/f188-api-library-vds-k8s）

- 証明書の変化検知を「mtime が新しくなったら」から「mtime・実体（dev/ino）・サイズのどれかが変わったら」に変更
  （`tls_reload::PemStamp`）。Kubernetes Secret の `..data` 付け替えで差し替え先の mtime が同じ・古い場合も追従する。
- Docker secrets の compose 例と E2E（`examples/container/`）、Kubernetes Secret ボリューム（`deploy/kubernetes/`）。
- 単体テスト（付け替え先の mtime が古い場合）、standalone E2E（TLS ハンドシェイクで返る証明書が替わる）、compose E2E（PASS）、
  kind（musl / glibc × plain / Istio / Linkerd すべてで Secret 更新後に新しい証明書を返す）。
