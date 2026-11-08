# B-347: 自己完結型 E2E の f271 が、F-293 の既定（Proxy-Wasm の失敗で拒否）より前の応答を期待して失敗する

**状態: 修正済み（feat/v0120-perf、2026-11-08）**

v0.12.0 の最終の検証で、自己完結型 E2E（`cargo test --test standalone_e2e --features full`）を回して見つけた。

## 事象

`f271_wasm_respond_body_phase_reuse_and_data_files` が失敗する。`/trap`（要求ヘッダの段でトラップするフィルタを付けた
`type = "Respond"`・`status = 503`・`body = "fallback"` のルート）に `(503, "fallback")` を期待し、`(503, "")` が返る。main と
v0.11.0 のタグでも同じ。veil の挙動は F-293 の仕様どおり（`failure_policy = "fail_closed"` は本文の無い 503）で、試験の側の不具合。

## 原因

- F-293（v0.11.0）で Proxy-Wasm の失敗の既定を `fail_closed` にしたとき、この試験の期待値を直していなかった（それまでは、
  トラップするとモジュールを飛ばしてルートの固定応答に落ちていた）。
- 自己完結型 E2E は CI（`ci.yml` は E2E を走らせない）でも、リリース前の全プラットフォームの試験
  （`docs/perf/platform_verification.md`）でも回していなかった。AGENTS.md の「ビルド・テスト」にも無く、v0.11.0 から失敗に気づかなかった。

## 対応

- 期待値を本文の無い 503 に直す（ルートの `"fallback"` に落ちないことを確かめる形）。トラップの後に新しいインスタンスになることの
  確認はそのまま。
- AGENTS.md の「ビルド・テスト」に自己完結型 E2E のコマンドを足し、全プラットフォームの試験の表にも載せる。

## テスト

`cargo test --test standalone_e2e` を `full`・`full,epoll` の両方で回し、全件通ることを確かめる（結果は下に追記する）。
