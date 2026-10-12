# F-190: Tower 互換・tokio インターフェース互換レイヤー（API ライブラリ）

**状態: 進行中（feat/f188-api-library-vds-k8s）**

## 目的

Tower の `Service` / `Layer` をハンドラに適用できるようにする（全体・パスプレフィックス単位）。tokio 前提のライブラリ
（tower-http の Timeout、sqlx、reqwest 等）もハンドラから使えるよう、tokio ランタイムへの橋渡しを用意する。
データプレーン（受付・プロトコル処理）には tokio を持ち込まない。

## 設計

- feature `tokio-interface`。veil `Request` ⇔ `http::Request<Full<Bytes>>` の変換、`Router::layer` / `layer_on(prefix, layer)`。
  サービスはワーカースレッドごとに組み立ててリクエストごとに clone（veil ランタイム上で実行）。
- `tokio_compat::run(fut).await`: 遅延起動する tokio マルチスレッドランタイムで `Send` な Future を実行し、完了を
  offload と同じ「結果スロット + eventfd/pipe 通知」で veil 側へ返す（スレッド越しの Waker 呼び出しはしない）。
  `Router::tokio_layer_on(prefix, layer)` は層ごと tokio 上で実行する。

## 受け入れ条件

- 単体/E2E: 特定プレフィックスだけに認証レイヤーが掛かる、tower-http の層（SetResponseHeader・Timeout）が動く、tokio の
  `sleep` を含むハンドラが動く。feature 無効時に tokio が本体依存に入らない。
