# B-427: `cache` feature の無いビルドの HTTP/2・HTTP/3 の静的配信が、ファイルを丸ごと読む量を数えていなかった

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Builds without the `cache` feature read every HTTP/2 and HTTP/3 static file fully into memory with no limit on
concurrent reads」から確認。

## 事象

- HTTP/2・HTTP/3 の静的配信は本体を丸ごと読む。`cache` のあるビルドは B-321 で、キャッシュに入らない読み込みを
  プロセス全体で 256 MiB までに抑えている（`UNCACHED_INFLIGHT`。超えたら `StaticFileOutcome::Busy` で 503）。
- `cache` の無いビルド（既定の feature 等）のスタブ（`cache::open_and_read_uncached`）は数えずに読んでいた。
  大きなファイルを 1 つの HTTP/2 の接続の多数のストリームから要求し、受信窓を閉じておくと、ストリームごとに
  ファイル全体がメモリに残り、少ない接続で OOM にできた。

## 対応

- 予約の仕組み（`UncachedRead`・`charge`）を常にコンパイルする `cache::inflight` へ移し、スタブも読む前に
  ファイルの大きさを予約する（上限に達していれば `Busy`。予約は本文を送り終えて落とすまで持つ）。
- 利用者ガイド（`configuration.md` の英日）に書いた。

## テスト

- `cache::inflight::tests::uncached_read_budget`（両方のビルドで走る）。`cache` の無いビルドは
  `cargo check`（既定・`--no-default-features`）でコンパイルを確かめる。
