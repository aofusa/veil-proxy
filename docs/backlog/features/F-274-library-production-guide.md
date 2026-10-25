# F-274: API ライブラリ（Rust・Python）の本番運用ガイドと推奨設定

**状態: 未着手（2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)。HttpArena の framework（standard）の前提）**

## 背景

- HttpArena の framework の standard モードは「**フレームワークの公式の本番ガイドが推奨する設定** だけを既定から変えてよい」
  （`frameworks/standard.md`。ワーカー数をコア数に合わせる・プールの大きさは可。文書に無いフラグ・検証やエラー処理を外す設定は不可）。
- veil のライブラリには「本番でどう設定するか」の文書が無い。既定は開発向け: アクセスログが有効（1 要求 1 行、F-244）、
  OpenAPI・Swagger UI が有効、待ち受けは `127.0.0.1:8080`（コンテナの中ではブリッジ網から届かない。HttpArena の validate は
  ブリッジ網 + `-p` で動く）。
- 既定を変えるのではなく（開発時の分かりやすさのための既定。F-244・F-197 の判断）、本番の推奨を文書で示す。

## 設計（文書の構成）

`docs/guide/library-production.md`（英日、新規）と `crates/veil-python/docs/production.md`（英日、新規）。内容:

| 項目 | 推奨 | 根拠・注意 |
|---|---|---|
| 待ち受け | コンテナ・公開サーバでは `0.0.0.0`（か具体的なアドレス）、複数ポートは `[[listener]]`（F-260） | 既定の `127.0.0.1` は開発用 |
| ワーカー数 | `threads = 0`（CPU の affinity・cgroup のクォータに合わせる既定）。cpuset で絞る環境はそのまま | `num_cpus::get()` が affinity を反映することを書く |
| アクセスログ | 高い要求率では `access_log.enabled = false` か、ログ基盤へ送る形式（JSON）で別の場所へ。要求ごとの stdout は 1 行の書き込みの費用がある | 計測値を載せる（有効・無効の差） |
| OpenAPI | 公開の API では `openapi_enabled = false`、または `/docs` を管理用のリスナー・IP に絞る（`[route.security]`） | 攻撃面を減らす |
| 圧縮 | `[api] compression = true`（F-270）と推奨のレベル（計測で決めた br・gzip の値） | |
| HTTP/3 | `[[listener]] http3 = true`、QUIC は既定の設定のまま | |
| Python | 軽い同期のハンドラは `offload=False`、I/O 待ちは `async def`（常駐のループ、F-263）、CPU の重い処理は `run_in_threadpool`・マルチプロセスは非推奨（理由と、どうしてもならリスナーを分けた複数プロセス）。`python_event_loops`・`handler_threads` の目安 | |
| ファイル記述子 | `LimitNOFILE`・`--ulimit nofile` の目安（WebSocket・長い接続） | `raise_nofile_limit` は hard まで上げるだけ |
| 停止 | `shutdown_timeout`、WebSocket・長い接続の扱い（F-261・F-262） | |
| TLS | 証明書のリロード、`[tls_client] ca`（F-259） | |
| メモリ | アロケータの選び方（`handler` では veil が `#[global_allocator]` を定義しない） | |
| 観測 | `[prometheus]`、ヘルスチェックのエンドポイントの書き方 | |

- 例: 本番向けの設定一式（Rust のビルダ・`config.toml`・Python の `veil.run`）を `examples/api/production/` に置き、`library_e2e.sh` で起動確認する。
- ベンチマークの提出物（F-269）は、このガイドに書いた設定だけを使う。

## 試験

- 例の起動確認（`library_e2e.sh`）。文書の中の TOML を `veil -t` で検査（F-255 の仕組み）。

## 受け入れ条件

- 英日のガイドが公開され、`docs/guide/library.md`・Python の文書の索引から辿れる。`backlog.md` が更新されている。
