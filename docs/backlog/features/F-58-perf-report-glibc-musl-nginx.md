# F-58: パフォーマンス測定レポート（glibc / musl / nginx 比較）

## 出典

`docs/artifacts/performance_report_veil_vs_nginx.md`（旧レポート。バグ修正前で
HTTP/1.1・musl が全滅していたため再測定が必要）。

## 概要

B-13 / B-14 / B-15 の修正後、glibc 版・musl 版・nginx(alpine) をコンテナ間通信で比較し、
`docker/assets/www/index.html`（同一ファイル）を配信して詳細なパフォーマンスレポートを作成する。

## 改修内容（測定内容）

1. glibc / musl 両イメージを full features（実運用は ktls/http2/mimalloc 相当）で再ビルド。
2. `config.toml` の組み合わせを網羅的に用意（ktls on/off、http2 on/off、reuseport balancing
   cbpf/kernel、open_file_cache on/off 等、ktls/http2/mimalloc が効く設定のみ）。
3. wrk（HTTP/1.1）・h2load（HTTP/2）をコンテナで実行し、コンテナ間通信で測定。
4. nginx(alpine) を同条件（同一 index.html・自己署名 ECDSA 証明書）で測定しベースライン化。
5. スループット・レイテンシ分布・エラー数・転送量・CPU/RSS 等を収集し、まとめ・考察付きで
   `docs/artifacts/` にレポート化。

## 受け入れ条件

- [ ] 全組み合わせで Non-2xx = 0（正常配信）を確認したうえで比較値を掲載。
- [ ] nginx / glibc / musl の 3 者比較表と考察を含む。

## 対応状況: 進行中