# F-193: C API（veil-capi）— C からライブラリとして呼び出す

**状態: 完了（feat/f188-api-library-vds-k8s）**

## 目的

C 言語から veil を共有/静的ライブラリ（`libveil.so` / `libveil.a`）として呼び出し、設定の読み込み・上書き、
ルート登録（関数ポインタ + user_data）、起動・停止を行えるようにする。

## 設計

- 別クレート `crates/veil-capi`（`crate-type = ["cdylib", "staticlib"]`）。本体の crate-type は変えない（F-184 の教訓）。
- ヘッダ `crates/veil-capi/include/veil.h`（cbindgen で生成してコミット）。エラーは整数コード + `veil_last_error()`。
- C のハンドラは既定でハンドラ用プールへ offload して実行（`VEIL_ROUTE_INLINE` でワーカー上で直接実行）。panic は 500。
- ライブラリはグローバルアロケータを持たない（ホスト側のアロケータと混在させない）。

## 受け入れ条件

- C サンプル（`examples/api/c-hello`）を gcc でコンパイル・リンクし起動、curl で応答を確認する E2E。

## 結果（2026-10-12、feat/f188-api-library-vds-k8s）

- `crates/veil-capi`（`libveil.so` / `libveil.a`、`include/veil.h`）。既定 offload、`VEIL_ROUTE_INLINE`、panic・非 0 は 500。
  cdylib で残る quiche の BoringSSL 名（`sk_num`/`sk_value`）を aws-lc の `OPENSSL_sk_*` へ転送する互換シムを持つ。
- E2E: 動的・静的リンクの両方で全ルート（HTTP/1.1・h2c）と OpenAPI。zip の成果物を Debian 11（glibc 2.31）でも確認。
