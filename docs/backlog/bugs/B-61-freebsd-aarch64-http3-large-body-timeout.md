# B-61: FreeBSD/aarch64 実機で `test_http3_large_request_body` が単体実行でも 60 秒タイムアウトする

**状態: 完了（再現せず。feat/v080-limitations で再検証）**

## 事象

FreeBSD 14.3 arm64（実機、QEMU/HVF on Apple Silicon）で `tests/e2e_setup.sh test`
（`--no-default-features --features full-freebsd`）を実行すると、542 テスト中
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

## 追記（2026-08-07）: Linux x86_64 でも再現する（FreeBSD/aarch64 固有ではない）

F-145 の検証中に、**Linux x86_64・`--features full`（io_uring バックエンド）でも同じ
タイムアウトが再現する**ことが判明した。したがって本チケットの表題にある
「FreeBSD/aarch64 固有」という前提は誤りで、プラットフォーム非依存の問題である。

同一コミット（F-145 適用前の親コミット）で本テストのみを単体実行した結果:

| 実行 | 結果 |
|---|---|
| 親コミット・単体実行 1 回目 | **FAILED**（60.01 秒でタイムアウト） |
| 親コミット・単体実行 2 回目 | ok（0.62 秒） |
| F-145 適用後・単体実行 | ok（0.57 秒） |

つまり **同一コードで成否が変わるフレーキー**であり、成功時は 0.6 秒で終わるのに対し
失敗時は 60 秒のタイムアウトまで一切進まない（`HTTP/3 connection closed: Timeout`）。
「遅い」のではなく「ある確率で完全に停止する」挙動である点が重要で、
QUIC のハンドシェイク/フロー制御のどこかでデッドロックしていることを示唆する。

参考: 同じ検証で自作 HTTP/3 クライアント（`tools/perf/h3load`、F-145）を実装した際、
**`quiche::Connection::on_timeout()` を呼ばないとハンドシェイクが確実にデッドロックする**
という事象に遭遇した（クライアントが ACK 遅延タイマーを発火させず、サーバはアドレス検証前の
増幅制限（受信バイト数の 3 倍）に達して続きを送れないまま相互に待ち続ける）。本バグも
「タイマー起動漏れによりフロー制御ウィンドウ更新が止まる」類の可能性があり、調査の入口として
サーバ側のタイマー処理（`src/http3_server.rs` の `on_timeout` 呼び出し条件）を挙げておく。

## 切り分け済みの事実

- WASM とは無関係（本テストは wasm フィルタを経由しない経路）。
- B-55（wasm vendoring）の変更前から存在していた潜在問題で、`full-freebsd`
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
  発見は B-55 で追加した `full-freebsd` の実機 E2E 実行が契機）
- F-140（NetBSD 対応。BSD 系プラットフォームの実機検証プロジェクトの一環）

## 2026-08-11 追記: FreeBSD **x86_64** でも再現。F-150/F-151 の前後で A/B し「先行して存在するバグ」と確定

F-150/F-151（rustls 送信の writev 化 / HTTP/3 メインループのイベント駆動化）の
検証中に FreeBSD 14.3-RELEASE **amd64**（QEMU/KVM、`full-freebsd`）のフル E2E で
本テストが失敗したため、**改修前後のコミットで同一手順の A/B** を取った
（`TEST_FILTER=test_http3_large_request_body TEST_THREADS=1`、単独実行 × 3 回）。

| コミット | 結果 |
|---|---|
| 改修前（`466aca2`、F-150/F-151 適用前） | **FAILED / FAILED / FAILED**（3 回とも 60.00 秒でタイムアウト） |
| 改修後（F-150 + F-151 適用後） | FAILED / FAILED / **ok**（33.06 秒） |

- **改修前が 3/3 失敗**しているため、本件は F-150/F-151 とは無関係の**先行バグ**である
  （むしろ改修後は 3 回中 1 回成功しており、悪化はしていない）。
- 失敗時は必ず **ちょうど 60.00 秒**（テストのタイムアウト値）で、その間まったく進捗しない
  ＝ 完全停止である点も従来の観測と一致する。
- **アーキ非依存**であることが確定した（既知の aarch64 に加えて x86_64 でも再現。
  2026-08-07 追記の「Linux x86_64 でも再現」と合わせ、**OS・アーキともに非依存の
  フレーキー**と考えるのが妥当）。
- なお同じフル E2E で同時に失敗した `test_error_handling_oversized_header` /
  `test_b17_bad_backend_no_response_returns_504` は、**ホスト負荷が高い状態**
  （並行 docker ビルドにより loadavg 8 超）でのみ失敗し、静穏時の再実行では
  543 passed / 1 failed（本件のみ）となった＝既知の負荷フレーキー。


## 2026-08-17 追記（F-155/F-156 の検証中の観測）

FreeBSD 14.3 aarch64 でフル E2E を計 4 回実行した際の再現状況:

- `test_http3_large_request_body`: **4 回中 4 回失敗**。単独実行でも
  1 回目は成功（30.41s）、別の機会には 60 秒タイムアウトで失敗しており、
  本チケットに記録済みの「単独実行でも再現するが intermittent」という
  性質と一致する。
- **同じ HTTP/3 の大きなリクエストボディ系である
  `test_http3_request_body_streaming_tls_backend`（F-44、1.2MB ボディ）でも
  4 回中 1 回失敗した**（単独実行では成功）。本チケットと同じ事象の
  別の現れ方である可能性が高く、調査時は両方を対象にすること。

いずれも F-155（capsicum パス解決・kqueue write hint・バッチ accept・sf_hdtr）
および F-156（L4 マルチワーカー化・H2C accept）とは無関係の経路であり、
これらの変更による回帰ではない（同一コードで Linux io_uring の E2E は 544 件全成功）。

## 再検証（2026-10-09、feat/v080-limitations）

Linux x86_64（io_uring）で `test_http3_large_request_body` を単独で 10 回続けて実行し、**10/10 成功**
（0.19〜0.39 秒）。フルスイートでも io_uring / epoll とも成功。F-145 以降の HTTP/3 の改修
（B-12 系の EOF 伝播・F-151 のダーティ集合・B-97）で解消したと考えられる。FreeBSD/aarch64 は
F-176 の BSD 検証（`bsd-vm.sh freebsd aarch64 e2e`）で確認する。
