# F-235: `App::tokio_service`（tower / axum のマウント）の変換のアロケーションを減らす

**状態: 完了（feat/v080-release、2026-10-16。目標の 85% には届かず、残りの内訳を下に記録して閉じる）**

## 目的

監査の報告によると、`App::tokio_service`（`src/api/tower.rs`）で既存の tower の Service（axum の Router 等）をマウントすると、
次のようになる。

- `veil::api::Request` と `http::Request<Full<Bytes>>` の相互変換で、要求あたり約 26 個のアロケーションが増える。
- 速度はネイティブのハンドラの約 67%（45,765 req/s）になる。

これを 85〜90% にする。

報告の Phase 3 のほかの項目は、すでに別のチケットにある。

- 6 の mTLS・リトライ: F-217・F-219 で実装済み。TOML の残りは F-226。
- 7 の HTTP/3 の io_uring と epoll の差: F-214。

## 設計

- **段階 0 で今の数を測り直す。** 報告の数字は F-202（ハンドラの経路を 14 → 4 allocs/req）より後か、確かめられていない。
  - 測り方: `tools/api-bench`（axum-veil）と F-202 の `allocs.py`・`ALLOC_TRACE`。
  - 結果: アロケーションの内訳（どの変換の何か）を表にして、このチケットに書く。
- **予想される内訳と対策**（段階 0 の表で確定させる）:
  - `http::HeaderMap` の構築: 要求のヘッダ数で容量を予約する。名前は `HeaderName::from_static` の表で、既知のヘッダを
    アロケーション無しにする。値は `HeaderValue::from_maybe_shared(Bytes)` で、要求バッファの `Bytes` を共有する
    （コピーしない）。
  - `Uri` の構築: `Bytes` から `Uri::from_maybe_shared` で作る。パスの文字列を作り直さない。
  - 本文: 要求の本文の `Bytes` を、そのまま `Full<Bytes>` に渡す。応答の本文は、`http_body::Body` の `poll_frame` から得た
    `Bytes` を、そのまま veil の応答の本文にする。
  - 応答の `HeaderMap` から veil の `Vec<(Bytes, Bytes)>` へ: スレッドローカルのスクラッチの `Vec` を再利用する。
    `HeaderValue` の `Bytes` 共有は、`http` クレートの API で取れるかを段階 0 で確かめる。取れなければコピーを 1 回に抑える。
  - `Extensions`・tower の `Service::call` の Future の `Box`: axum の Router は `call` ごとに Future を `Box` にする。
    これは veil の外のコストなので、減らす対象から外す。
- 変換は `src/api/tower.rs` の中に閉じる。ネイティブのハンドラの経路（`Backend::Handler`）には触れない。

## 受け入れ条件

- 段階 0 の内訳の表と、改善後の allocs/req・req/s（ネイティブのハンドラとの比）を、`docs/guide/library.md`（英日）の性能の
  節に書く。
- 目標: ネイティブのハンドラの 85% 以上。届かない場合は、残りの内訳を書いて閉じる。
- ネイティブのハンドラの経路と、既存のプロキシの性能が退行しない（`suite_ab.sh`・`tools/api-bench`）。
- `tests/api/library_e2e.sh` の tower の試験と、`nonregression`（full のバイナリに API ライブラリのコードが入らない）が通る。


## 実装の結果

### 段階 0（測り直し、改修前）

`allocs.py`（wrk の負荷の前後の差を要求数で割る）: ネイティブ 4.00、マウント 30.00 allocs/req。報告の「約 26 増える」と一致した。
`ALLOC_TRACE` で 1 要求の確保を追うと、veil 側は次のとおりだった。

| 場所 | 回数 | 対策 |
|---|---:|---|
| `to_http_request` の URI（`String` → `Bytes` へもう一度） | 2 | クエリが無ければパスの `Bytes` をそのまま、あれば 1 回の連結 → 0〜1 |
| ヘッダの値の複製（`HeaderValue::from_bytes`） | ヘッダ数 | `HeaderValue::from_maybe_shared` で共有 → 0 |
| extensions への veil の `Request`（Box と表） | 3 | マウントしたサービスは読まないので入れない → 0（`layer` の経路は入れたまま） |
| 応答の本文の `collect()`（1 フレームでもキューを確保） | 1 | フレームが 1 つならその `Bytes` を使う → 0 |
| 応答のヘッダの名前の複製 | ヘッダ数 | よく使う名前は表で静的な `Bytes` に → 0（`http` 1.x には `HeaderName`・`HeaderValue` を `Bytes` へ移す API が無いので、値は 1 回複製する） |
| `HeaderMap`（索引と本体）・応答のヘッダの `Vec`・ハンドラの Future の Box・tokio 互換の Waker・パスのパラメータ | 6 | 残す（`HeaderMap` は axum が要る、他はネイティブと共通か tokio 互換の仕組み） |

### 結果（同じ回の交互の計測、2 スレッド、wrk -t2 -c64）

| 対象 | plaintext req/s | json req/s | CPU µs/req | allocs/req |
|---|---:|---:|---:|---:|
| veil のネイティブハンドラ | 65,945 | 61,807 | 30.9 | 4 |
| マウント（改修後） | 49,960 | 46,013 | 41.5 | 22 |
| （参考）tokio 上の axum（スレッドごと） | 66,587 | — | 30.5 | — |
| マウント（改修前、docs/guide/library.md の表） | 45,765 | 41,560 | 44.5 | 30 |

- ネイティブに対する比は 67% → 76%（plaintext）・74%（json）。目標の 85% には届いていない。
- 残りの 22 回のうち約 12 回は axum の中（`OriginalUri`・`MatchedPath`・`UrlParams` の extensions、ルートの Box、axum の本文の型）。
- perf（特権のコンテナから。ホストは `perf_event_paranoid=4`）で見ると、平らで、目立つ一点が無い。差の 10 µs/req は、
  axum のルーティングと変換と確保に分かれている。
- これ以上は axum を通さない（ネイティブのハンドラにする）しかない。`docs/guide/library.md`（英日）の性能の節に書いた。

## 関連

F-190、F-198、F-202、F-205、`src/api/tower.rs`。
