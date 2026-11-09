# B-356: 試験用クレートの libsqlite3-sys の勧告で cargo-audit が失敗する

**状態: 対応済み（feat/v0120-perf、2026-11-10）**

v0.12.0 の最終の検証（`tools/container_security` の cargo-audit）で見つけた。

## 事象

`cargo audit -f tools/tokio-compat/ecosystem/Cargo.lock` が `RUSTSEC-2026-0459`（libsqlite3-sys 0.30.1 が同梱する C の SQLite の CVE-2026-119375。
中程度 6.8）で失敗する。container_security の `cargo_audit` フェーズが失敗になる（2 日前の同じ検査は通っていた。勧告は 2026-10-30 付けで、その後データベースに載った）。

## 影響

無し。該当のクレートは `tools/tokio-compat/ecosystem`（tokio 互換層の実ライブラリ試験、`publish = false`）の sqlx の SQLite 経路だけで、veil の本体・配布物の依存には入らない
（`Cargo.lock` の本体は勧告に一致しない）。試験は `sqlite::memory:` に固定の SQL を流すだけで、外から入力を受けない。

## 対応

`.cargo/audit.toml` に理由付きで除外する（AGENTS.md の方針「直せないものは依存を替えて取り除き、やむを得ず残すときは理由付きの ignore」）。

- 修正版（libsqlite3-sys 0.39 以上）は選べない。sqlx の最新（0.9.0）へ上げても `sqlx-sqlite` が libsqlite3-sys を `>=0.30.1, <0.38.0` に固定している（Mac で確かめた）。
- SQLite の経路を試験から外すと、既定（`VEIL_PG_URL` 無し）で sqlx の試験が 1 つも走らなくなる（tokio 互換層が SQLite の専用スレッドの形を確かめている）ので外さない。
- sqlx が 0.39 以上に対応したら上げて、除外を消す。

## 確認

aofish の `tools/container_security`（cargo_audit を含むフル）で確かめる（[platform_verification.md](../../perf/platform_verification.md)）。
