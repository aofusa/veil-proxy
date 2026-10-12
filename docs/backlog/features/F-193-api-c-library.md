# F-193: C API（veil-capi）— C からライブラリとして呼び出す

**状態: 進行中（feat/f188-api-library-vds-k8s）**

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
