# F-77: プロトコル差分（differential）テスト

親: [F-72](F-72-security-testing-further-hardening.md) 項目 3。

## 目的

同一リクエストを Veil と nginx/envoy に流し、ステータス・ヘッダー正規化・
チャンク処理の差分を比較して曖昧な解釈（スマグリングの温床）を検出する。

## 実装済み（2026-07-06）

- **`tools/container_security/security/run_differential.sh`**（自己完結トポロジ、docker のみ、
  既定 `SKIP_DIFFERENTIAL=1`）。共有バックエンド（python 最小 echo）を **Veil と nginx の
  双方**でフロントし、同一の crafted リクエスト集合（正常 GET/POST・chunked・CL+TE・複数 CL）を
  両者へ送ってステータスを比較する。
- **既知の意図的差分は allowlist 化**: Veil はスマグリング要因（CL+TE・複数 CL）を
  [B-23](../bugs/B-23-request-smuggling-cl-te.md) で厳格に 400 拒否するため、nginx との
  差分は「想定内（Veil がより厳格）」として扱い、**それ以外の予期しない差分のみ**を
  警告・起票対象とする。
- `run.sh` フェーズ 4i + `report.sh` に配線（`DIFFERENTIAL_BLOCKING=1` で差分検出時に失敗化）。

## 残件

- **envoy** を第 3 のプロキシとして追加（現状は Veil vs nginx の 2 者）。
- ヘッダー正規化（大文字小文字・obs-fold・重複ヘッダー結合）の詳細差分比較
  （現状はステータスコード中心）。

## 受け入れ条件

- [x] 代表リクエスト集合に対する差分レポートが docker のみで生成できる（Veil vs nginx）。
- [x] 予期しない差分を警告し、既知差分は allowlist 化（backlog 起票フロー）。
