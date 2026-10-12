# バックログ: 動的設定配信 API（VDS / xDS 相当）

## 名称

- 文脈上 **「VDS（Veil Discovery Service）」** などプロジェクト内名称を付与可能。
- プロトコル設計の参考は Envoy の **xDS**（LDS / CDS / RDS / EDS 等）ファミリー。

## 目的

- ファイル（`config.toml`）と SIGHUP に加え、**制御プレーンからストリームで設定を配信**できるようにする。
- 既存の **ArcSwap によるロックフリー設定スワップ**（[AGENTS.md](../../../AGENTS.md) の設計哲学・設計制約）と整合する内部モデルを維持する。

## スコープ案

1. **API 形態**
   - gRPC 双方向ストリーム（推奨案）または REST + 長輪詢 / SSE の比較検討。
   - 認証: mTLS、トークン、または Unix ソケット限定など。

2. **リソースモデル**
   - ルート、upstream クラスタ、TLS 設定、WASM フィルタ参照など、TOML スキーマと **1:1 で対応可能な中間表現**を定義。
   - 差分（Delta）配信 vs スナップショット全文のトレードオフ。

3. **安全性**
   - 不正な設定でプロセスを落とさない（検証失敗時は前版を維持）。
   - レート制限・最大リソース数。

## 非スコープ（初期）

- Envoy とバイナリ互換の xDS を完全実装すること（段階的互換またはサブセットで開始可）。

## 受け入れ条件（案）

- 参照実装クライアント（最小）と E2E で「配信 → 新リクエストに反映」を確認。
- 仕様書（proto または OpenAPI）がリポジトリに含まれる。

## 依存

- [grpc.md](grpc.md) の安定化、gRPC server 能力（Veil 側がサーバになる場合）。

---

## 決定した設計（2026-10-12、feat/f188-api-library-vds-k8s）

**状態: 進行中**

- 名称は **VDS（Veil Discovery Service）**。feature `vds`（`full*` に含める）、`[vds]` セクション。
- 転送は **xDS の REST-JSON（State-of-the-World）に準拠した長輪詢**: `POST /v1/discovery` に `DiscoveryRequest`
  （`version_info` / `node` / `type_url` / `response_nonce` / `error_detail`）を送り、`DiscoveryResponse`
  （`version_info` / `nonce` / `resources[]`）を受ける。変化が無ければサーバは保留し、タイムアウトで 304。
  ACK は新しい `version_info`、NACK は旧 `version_info` + `error_detail`（xDS と同じ意味論）。
  gRPC 双方向ストリームは採らない（データプレーンが tokio 非依存で、制御プレーン側も HTTP だけで実装できる方が運用しやすい）。
- リソース: `type.veil.dev/veil.v1.Route`（`[[route]]` と同じスキーマ）・`type.veil.dev/veil.v1.Upstream`（`[[upstreams]]` と同じ）を
  1 個のスナップショット（`type.veil.dev/veil.v1.Snapshot`）で配信（ADS 相当で整合性を保つ）。listen・TLS・L4 は対象外（再起動が必要）。
- 適用: VDS オーバーレイを設定ロードの単一チョークポイント（`config_override::apply_to_toml_str`）で当て、既存のリロード経路
  （検証 → ArcSwap）を通す。検証失敗は前版を維持して NACK。優先順位はファイル < VDS < 環境変数 < CLI。
- 制御プレーンとの通信は専用スレッド（データプレーン外）。認証は Bearer トークン（ファイル）・mTLS・追加 CA。
- 参照制御プレーン `tools/vds/vds_server.py`、仕様 `docs/guide/vds.md`（英日）+ JSON Schema。
