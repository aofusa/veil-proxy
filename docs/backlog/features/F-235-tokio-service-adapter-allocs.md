# F-235: `App::tokio_service`（tower / axum のマウント）の変換のアロケーションを減らす

**状態: 未着手（2026-10-16 起票・設計。出典: docs/artifacts/veil-comprehensive-review-and-comparison.md の §3.2・§5 Phase 3 の 8）**

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

## 関連

F-190、F-198、F-202、F-205、`src/api/tower.rs`。
