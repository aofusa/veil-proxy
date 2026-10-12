# コンテナでの証明書・設定の受け渡し（F-179 / F-180）

| ファイル | 内容 |
|---------|------|
| `config.toml` | 証明書を `/run/secrets` から読み、`auto_reload` で更新に追従する設定 |
| `compose.secrets.yaml` | Docker secrets で証明書・秘密鍵を渡し、`VEIL_OVERRIDE_<n>` で設定を上書きする compose |
| `gen-cert.sh` | 検証用の自己署名証明書を `secrets/` に作る |
| `e2e.sh` | 上記をビルド済みイメージで起動し、TLS 応答・環境変数の上書き・証明書の差し替えを確認する |

Kubernetes（Secret ボリューム・Istio / Linkerd のサイドカー）は [`deploy/kubernetes/`](../../deploy/kubernetes/) を参照。

詳しくは [docs/guide/tls.md](../../docs/guide/tls.md#certificates-and-keys-in-containers)・
[docs/guide/running.md](../../docs/guide/running.md)（環境変数による上書き）。
