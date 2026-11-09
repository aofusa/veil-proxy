# B-351: Windows で HTTP/3 のワーカーが 2 本目から bind に失敗し、秘密鍵のゼロ化が次のリロードまで遅れる

**状態: 修正済み（feat/v0120-perf、2026-11-09）**

v0.12.0 の Windows の検証（C の例のログ）で、起動のたびに `Server error: ... (os error 10048)` が 15 行（16 ワーカーのうち 15 本）出ていた。

## 事象

- Windows で HTTP/3 を有効にすると、ワーカー 1 本だけが動き、残りは `WSAEADDRINUSE`（10048）で止まって、起動のたびに `ERROR` を本数ぶん出す。
  応答はできる（1 本が受ける）が、利用者には起動に失敗したように見える。
- 止まったワーカーも証明書リロードの待ち人数（`pending_workers`）に数えられていたので、起動時に複製した秘密鍵の平文は、全員の適用が
  揃わず次のリロードまでゼロ化されなかった（`Drop` の安全網で終了時には消える）。

## 原因

- Windows には UDP の受信を複数のソケットへ分ける仕組みが無い（Linux の `SO_REUSEPORT`・FreeBSD の `SO_REUSEPORT_LB` に当たるものが無い）。
  `QuicUdpSocket::bind_reuseport` は普通の `bind` なので、2 本目から失敗する。`SO_REUSEADDR` で重ねても受信先が定まらず、QUIC の接続の状態は
  ワーカーの中にあるので使えない。
- `http3_server` の `register_http3_worker()` が `bind` の前にあった。

## 対応

- Windows の HTTP/3 はリスナーごとに 1 ワーカーにする（`listener_workers::http3_worker_count`。減らしたときは起動時に `info` で 1 行）。
  HTTP/3 の配布の待ち（PEM の複製）の人数は、実際に起動した本数（`spawn_http3_listener` の戻り値）で数える。
- `register_http3_worker()` を `bind` の後へ移す（どの OS でも `bind` に失敗したワーカーを数えない）。
- ガイド: `docs/guide/platforms.md`・`docs/guide/configuration.md`（日本語も）。

## テスト

- 単体: `listener_workers::http3_worker_tests::windows_runs_one_http3_worker_per_listener`。
- 自己完結型 E2E: `b351_http3_with_several_workers_starts_cleanly`（`threads = 3`・HTTP/3 有効で起動し、HTTP/3 の要求が通り、ログに
  `Server error` が無く、`workers=` が Windows は 1・ほかは 3）。
