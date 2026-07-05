# F-76: HTTP リクエストスマグリング専用テスト

親: [F-72](F-72-security-testing-further-hardening.md) 項目 2 / [F-66](F-66-dast-owasp-zap.md)。

## 目的

ZAP baseline（F-66、受動スキャン中心）ではカバーされない CL.TE / TE.CL /
H2C ダウングレードのスマグリングを能動検査する（プロキシ特有の高リスク領域）。

## 改修案

- `smuggler` / `h2csmuggler` 等の専用ツール、または Nuclei テンプレートを
  `tools/container_security/security/` に追加し、稼働中 Veil コンテナへ実行。
- Veil の framing 実装（Content-Length と Transfer-Encoding の同時指定拒否、
  H2C アップグレード制御）に対する差分応答を検査。
- 検出項目のトリアージを backlog に反映。

## 受け入れ条件

- docker のみで CL.TE / TE.CL / H2C ダウングレードのプローブが実行できる。
- Veil が曖昧な framing を一貫して拒否（400/クローズ）することを確認。
