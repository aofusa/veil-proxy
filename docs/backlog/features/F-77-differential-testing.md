# F-77: プロトコル差分（differential）テスト

親: [F-72](F-72-security-testing-further-hardening.md) 項目 3。

## 目的

同一リクエストを Veil と nginx/envoy に流し、ステータス・ヘッダー正規化・
チャンク処理の差分を比較して曖昧な解釈（スマグリングの温床）を検出する。

## 改修案

- `tools/container_security/` に Veil / nginx / envoy を同一バックエンドへ向けて起動し、
  生成した多様なリクエスト（境界ケース・不正 framing・大文字小文字・重複ヘッダー）を
  各プロキシへ送って応答を突き合わせるハーネスを追加。
- 差分を JSON レポート化し、既知の意図的差分は allowlist 化。

## 受け入れ条件

- 代表リクエスト集合に対する 3 者の応答差分レポートが docker のみで生成できる。
- 予期しない差分が backlog に起票される。
