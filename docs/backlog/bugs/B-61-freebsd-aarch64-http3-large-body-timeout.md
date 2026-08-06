# B-61: FreeBSD/aarch64 実機で `test_http3_large_request_body` が単体実行でも 60 秒タイムアウトする

**状態: 未対応（要調査）**

## 事象

FreeBSD 14.3 arm64（実機、QEMU/HVF on Apple Silicon）で `tests/e2e_setup.sh test`
（`--no-default-features --features full-freebsd-aarch64`）を実行すると、542 テスト中
540 が pass、次の 2 件が fail する。

1. `test_error_handling_oversized_header`
2. `test_http3_large_request_body`

`test_error_handling_oversized_header` は**単独実行すると pass**する
（`test result: ok. 1 passed; ... 541 filtered out`）。他プラットフォームでも
知られている負荷起因の既知フレーキーであり、本チケットの対象外。

`test_http3_large_request_body` は**単独実行しても 60 秒でタイムアウトして fail**する:

```
HTTP/3 connection closed: Timeout
test result: FAILED. 0 passed; 1 failed; ... finished in 60.01s
```

負荷起因ではなく、**FreeBSD/aarch64 固有の HTTP/3 大容量リクエストボディの問題**
であると考えられる。

## 切り分け済みの事実

- WASM とは無関係（本テストは wasm フィルタを経由しない経路）。
- B-55（wasm vendoring）の変更前から存在していた潜在問題で、`full-freebsd-aarch64`
  で E2E を実機で回したのが今回（2026-08-06）が初めてだったため今回顕在化した。
- 他プラットフォームの同テストは pass する（NetBSD x86_64 のフル E2E でも本テストは
  失敗していない）。

## 未調査（原因）

原因は未調査。調査の入口として以下を挙げる:

- quiche のフロー制御/ペーシング設定（大容量ボディ送信時のウィンドウ更新タイミング）。
- reactor(kqueue) 経路の UDP 送受信バッチ処理（`src/runtime/reactor/`）。FreeBSD の
  kqueue 実装・UDP ソケットバッファのデフォルトが aarch64 のみで異なる可能性。
- aarch64 での MTU/GSO 無効時の挙動（GSO は Docker 環境等で無効化されることが
  project memory にあり、実機 FreeBSD/aarch64 でも同様の経路を通る可能性）。
- E2E クライアント側（quinn/h3）の挙動。クライアント側のタイムアウト設定・
  ストリーム制御が aarch64 の低速な TCG/実機環境で想定と異なる可能性。

## 関連

- B-50（FreeBSD の HTTP/3 UDP バインド問題）
- B-55（wasmtime が BSD の一部プラットフォームをサポートしていない件。本チケットの
  発見は B-55 で追加した `full-freebsd-aarch64` の実機 E2E 実行が契機）
- F-140（NetBSD 対応。BSD 系プラットフォームの実機検証プロジェクトの一環）
