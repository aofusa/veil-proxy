# B-443: メッシュの Istio のメトリクスの表が、クライアントが書ける peer metadata の id ごとに系列を作って捨てず、際限なく伸びた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Istio metric table grows without bound per request: entries are keyed by Arc pointer and fed by peer-metadata headers the client controls」から確認。

## 事象

- `mesh::stats` はワーカーの表を相手のワークロード・SPIFFE ID・サービスの `Arc` のポインタで引き、項目がその `Arc` を生かしたまま捨てなかった。inbound の相手のワークロードは要求の `x-envoy-peer-metadata-id`・`x-envoy-peer-metadata` から作る（id ごとに新しい `Arc`）。
- inbound のポートへ届く者（メッシュの任意のワークロード、PERMISSIVE なら平文のクライアントも）が id を毎回替えた要求を送ると、要求ごとにラベルの文字列とヒストグラム 3 つの項目が増え続け、メモリを使い切れた。mTLS の接続ごとに作る SPIFFE ID の `Arc` でも、正常な接続の入れ替わりで同じく伸びた。

## 対応

- 系列はラベルの文字列の内容でまとめる（ポインタのキーは初めてのときだけラベルを作って内容の系列を引く速い引き当てとし、8,192 で捨てて作り直す）。ワーカーの系列は 2,048 までで、超えた新しい相手はワークロード・プリンシパルを `unknown` にした系列へまとめる（その枠 512 も尽きたら記録しない）。利用者ガイド（`mesh.md` の英日）に書いた。

## テスト

- `mesh::stats::tests::series_are_keyed_by_content_and_capped`。
