# F-201: API ライブラリの比較ベンチマーク（axum・FastAPI）と axum の互換確認

**状態: 完了（feat/api-bench-axum-fastapi）**

## 要望

- tokio 互換レイヤー（F-198）で axum が動くか確かめる。動くなら、`veil::tokio` 上の axum と本物の tokio 上の axum を比べる。
- Python からの veil 利用を FastAPI のアプリと比べる。
- 結果をドキュメントに反映する。

## 実施内容

- `tools/api-bench/`: 同じ 2 エンドポイント（`/plaintext`・`/json/{name}`）を、axum + tokio・axum + `veil::tokio`（同じソースで
  tokio を shim に置き換え）・veil の `App` に axum の `Router` を `tokio_service` で載せたもの・veil のネイティブハンドラ・
  FastAPI（uvicorn）・veil の Python で実装し、`run_bench.py` で交互に計測（サーバとクライアントを別の物理コアに分ける、
  プロセスの CPU とサーバのコアの busy から 1 要求あたりの CPU 時間を出す）。
- `tools/tokio-compat/ecosystem` に axum（`axum::serve`・抽出子・State・30 並行・graceful shutdown）を追加。io_uring・epoll とも ok。
- `tools/tokio-compat/run_tokio_tests.py`: タイムアウト・中断のとき試験のプロセスグループごと止める（取り残された試験が
  2 本、9 時間 CPU を使い続けて計測を乱していた）。

## 結果（i5-3320M、2 コア 4 スレッド。サーバ = CPU 0-1・2 スレッド、wrk = CPU 2-3、`-t2 -c64`、3 ラウンドの中央値）

数値は `docs/guide/library.md` の Performance 節（英日）。要点:

- axum は `veil::tokio` でそのまま動く。スレッドごとのリスナー（`SO_REUSEPORT`）で tokio 上の axum の約 80%（io_uring）〜
  84%（epoll）、1 要求あたりの CPU は 20〜25% 多い。
- `axum::serve` をマルチスレッドの `veil::tokio` ランタイムで動かすと 1 スレッドしか使わない（work stealing が無く、
  接続のタスクは受理したスレッドに留まる）。ガイドの「tokio との違い」に書き、スレッドごとのリスナーか `App` を案内。
- veil のネイティブハンドラは tokio 上の axum の 63%（io_uring）/ 72%（epoll）、`tokio_service` で載せた axum は 50〜55%。
  API ライブラリのハンドラ経路が重い（F-202）。
- Python: veil は FastAPI（2 ワーカー）の 2.1〜2.5 倍、1 要求あたりの CPU は半分以下、RSS は 4 分の 1 以下。

## 計測中に見つけた不具合

- B-122（平文リスナーのプロトコル判別が最初のバイトを 200ms しか待たず、HTTP/1.1 に GOAWAY を返していた）。修正済み。
