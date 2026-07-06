# F-76: HTTP リクエストスマグリング専用テスト

親: [F-72](F-72-security-testing-further-hardening.md) 項目 2 / [F-66](F-66-dast-owasp-zap.md)。

## 目的

ZAP baseline（F-66、受動スキャン中心）ではカバーされない CL.TE / TE.CL /
H2C ダウングレードのスマグリングを能動検査する（プロキシ特有の高リスク領域）。

## 実装済み（2026-07-06）

- **能動プローブ `tools/container_security/security/run_smuggling.sh`**（docker `python:3-slim`
  標準ライブラリのみ）。稼働中 Veil コンテナへ TLS 経由で crafted な生リクエストを送り、
  CL.TE / TE.CL / 複数 CL / 終端非 chunked TE が **一貫して 400 で拒否**されること、
  および単独 chunked が誤検知されない（非 400）ことを検査。`run.sh` フェーズ 4h +
  `report.sh` に配線（`SKIP_SMUGGLING` トグル、`SMUGGLING_BLOCKING=1` で失敗化）。
- **実行中に [B-23](../bugs/B-23-request-smuggling-cl-te.md) を検出・修正**:
  `Content-Length: 0` + `Transfer-Encoding: chunked`（CL 値に依らない CL.TE デシンク）が
  従来拒否されずバックエンドへ CL+TE 曖昧メッセージを転送していた。`classify_request_framing`
  純関数で一律 400 拒否 + chunked 時の CL 転送除去（多層防御）へ修正。
- **Rust レベル検証**: 単体 `http_utils::chunked_span_tests::framing_*`（6 件）+
  E2E `test_request_smuggling_cl_te_rejected` / `test_request_smuggling_legitimate_framing_allowed`。

## 残件

- **H2C ダウングレード**スマグリングの能動テスト（現状は HTTP/1.1 ベクタ中心。
  Veil の h2→backend は本文を再フレーミングするため CL/TE 透過はしないが、専用プローブは未整備）。
- `h2csmuggler` / Nuclei テンプレートの追加（外部ツールイメージ依存）。

## 受け入れ条件

- [x] docker のみで CL.TE / TE.CL のプローブが実行できる（`run_smuggling.sh`）。
- [x] Veil が曖昧な framing を一貫して拒否（400）することを確認（単体 + E2E + プローブ）。
