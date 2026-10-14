# F-202: API ライブラリのハンドラ経路の性能改善

**状態: 完了（feat/f202-api-handler-perf。io_uring のキープアライブ待ちの 2 段は F-205 へ）**

## 背景（F-201 の計測）

`tools/api-bench` の `/plaintext`（HTTP/1.1 キープアライブ、64 接続、サーバ 2 スレッド）で、veil のネイティブハンドラは
1 要求あたり 47.6 µs（io_uring）/ 41.4 µs（epoll）の CPU を使い、tokio 上の axum（29.8 µs）の 63% / 72% のスループット。
axum の `Router` を `tokio_service` で載せると 61.1 / 55.6 µs。プロキシのデータプレーンではなく、API ライブラリの
ハンドラ経路（`Backend::Handler`）の問題。

strace（5 秒、32 接続）で見た 1 要求あたりの syscall:

| 対象 | syscall / 要求 |
|---|---|
| ネイティブハンドラ（io_uring） | `io_uring_enter` 2.3 |
| ネイティブハンドラ（epoll） | `sendto` 1.0・`poll` 0.6・`read` 0.5・`epoll_ctl` 0.1 |
| axum + `veil::tokio`（epoll） | `recvfrom` 0.9・`writev` 0.5・`poll` 0.5・`epoll_ctl` 0.5 |

## 調べること

- io_uring で 1 要求あたり `io_uring_enter` が 2 回を超える理由（受信と送信を別々に submit + wait している、
  キープアライブの待ちで POLL_ADD → RECV の 2 段になっている等）。プロキシの HTTP/1.1 経路と同じ投入のまとめ方ができないか。
- 要求ごとのアロケーション（`Request` の組み立て・ヘッダ・パスパラメータ・`Response` の変換・ハンドラの `Box<dyn Future>`）を
  数えて減らす（`alloc-stats` ビルドで allocs/req）。
- `tokio_service` の `http::Request`/`http::Response` 変換（本文の `Full<Bytes>` 化）の複製を減らす。
- reactor の `poll(2)` の確認（B-75）がハンドラ経路で毎要求走っていないか。

## 受け入れ条件

- `tools/api-bench` でネイティブハンドラが tokio 上の axum と同等以上（同じ条件の交互計測、io_uring・epoll）。
- プロキシの性能（`tools/perf` の交互 A/B）に退行が無い。

## 実施内容（2026-10-13）

`tools/api-bench/allocs.py`（`--features count-allocs` のビルドで確保の累計を数え、wrk の要求数で割る）と、
同じビルドの `ALLOC_TRACE=<n>`（負荷中の確保 n 回ぶんのバックトレース）で、HTTP/1.1 のネイティブハンドラ 1 要求あたりの
確保を洗い出した（`/plaintext` で 14 回）。

| 確保 | 対応 |
|---|---|
| メソッド・パスの `Bytes::copy_from_slice`（＋パスの `clone` が Shared に昇格） | 要求ヘッダの領域を指す `Bytes` をそのまま渡す |
| ヘッダの `to_vec()` | `handle_backend` が `headers_for_proxy` を所有で受け取り、そのまま `Request` へ |
| `Request.headers` の `Arc::new` | `Vec` で持ち、`Headers` 抽出子が使われたときだけ `Arc` に包む |
| tokio 互換の `safe_waker` の `Arc`（要求ごと） | 接続ごとの `CallCache` に包んだ Waker を置き、キープアライブの要求をまたいで使い回す |
| 応答ヘッダの `Vec`（`http1_head`）と本文の 2 回目の書き込み | 要求バッファのプールのバッファにヘッダを書き、3KB 以下の本文は続けて 1 回で書く |
| `wasm` 無しのビルドで要求ごとの `Arc::new(Vec::new())` | スレッドごとの空の一覧を使い回す |
| ヘッダ位置の `Vec`・ルーティング用の `Vec<(&[u8], &[u8])>`・Content-Length の `Vec` | スタックの配列・イテレータ（await をまたがない。プロキシの経路にも効く） |

残りは `/plaintext` で 4 回（ヘッダの `Vec`・`handle_api_handler` の `Box`（プロキシ経路と同じく `handle_backend` の
Future を小さく保つため残す）・ハンドラの `Box<dyn Future>`・応答ヘッダの `Vec`）。

### 結果（交互計測、io_uring・epoll）

| 対象 | allocs/req | plaintext req/s | json req/s | CPU µs/req |
|---|---:|---:|---:|---:|
| 改修前 ネイティブ | 14 | 42,366 | 39,664 | 48.2 |
| 改修前 ネイティブ（epoll） | 14 | 48,783 | 44,921 | 41.5 |
| 改修後 ネイティブ | 4 | 64,546（+52%） | 58,524（+48%） | 31.7 |
| 改修後 ネイティブ（epoll） | 4 | 66,722（+37%） | 60,461（+35%） | 30.9 |
| 改修前 axum を `tokio_service` で載せたもの | 40 | 33,349 | 31,247 | 61.4 |
| 改修後 同上 | 30 | 45,765（+37%） | 41,560（+33%） | 44.5 |
| 参考: axum + tokio（per-core、同じ回） | – | 68,343 | 61,675 | 29.6 |

5 ラウンドの再計測（`docs/artifacts/f202_ab_r2.tsv`）で、tokio 上の axum に対して io_uring 93%（json 94%）・
epoll 98%（json 98%）。epoll は誤差の範囲で同等。

### 残り（F-205）

strace では、io_uring は 1 要求あたり `io_uring_enter` 約 3 回（strace 下）、epoll は `poll` 1・`read` 1・`sendto` 1。
キープアライブの次の要求を readiness 待ち（io_uring は `POLL_ADD`、epoll は確認用の `poll(2)`）→ 読み取り、の 2 段で
待つため（B-101 でアイドル接続に読み取りバッファを持たせないようにした設計）。プロキシと共通の経路なので F-205 で扱う。
