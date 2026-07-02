# F-39: HTTP プロキシ層の splice を io_uring 非同期 splice に統一

- **優先度**: P1
- **対応状況**: 完了（2026-07-02）

## 完了メモ

- `splice_body_transfer` を `runtime::splice`（IORING_OP_SPLICE）の非同期 Future ベースに書き換え。`src → pipe → dst` の両段を io_uring 化し、WouldBlock 時は `readable()` / `writable()`（POLL_ADD）で待機。
- 旧実装の潜在バグも解消: pipe → dst 段が WouldBlock で中断すると pipe 内残データと `remaining` カウントがずれてデータ損失し得たが、新実装は取り込んだ n バイトを必ず全量ドレインしてから次チャンクへ進む。
- `ktls_rustls.rs` の同期 `libc::splice` ラッパー（`splice` / `splice_transfer` / `SplicePipe::transfer`）を削除。`SplicePipe` はパイプ管理のみに縮小。
- `rg libc::splice src/` の実呼び出しゼロを確認。features full E2E 396 通過（失敗 1 件は既知の B-10 フレーキーで本件と無関係）。
- **出典**: `docs/artifacts/analysis_results.md` 改善案2（ホットパス絶対規則違反の是正）

## 機能説明・現状

`src/proxy.rs` の kTLS splice 転送（`splice_body_transfer` 系）は、非同期待機（`readable().await` / `POLL_ADD`）こそ行っているが、実際の I/O は `SplicePipe::transfer()` 経由で `libc::splice`（同期システムコール）を直接呼び出している。

これは AGENTS.md の **ホットパス絶対規則**「ホットパスのあらゆる I/O・システムコールは必ず非同期（io_uring）で行う」に明確に違反している。すでに L4 プロキシ（`src/l4/proxy.rs`）と `src/runtime/splice.rs` には `IORING_OP_SPLICE` を使う `splice(fd_in, fd_out, len).await` が存在するため、HTTP 側もこれに統一すべき。

## 改修内容

- `src/proxy.rs` の `libc::splice` 直接呼び出しを `src/runtime/splice.rs` の io_uring 非同期 splice (`.await`) に置き換える。
- pipe 経由 2 段 splice（`src -> pipe -> dst`）の両段を io_uring 化する。
- EAGAIN / 部分転送 / EOF・エラー処理を非同期経路で正しく扱う。

## 改修案

- L4 の実装（`src/l4/proxy.rs`）を参照し、HTTP のボディ転送ループを io_uring splice の `.await` ベースに書き換える。
- kTLS ソケット ⇔ pipe ⇔ バックエンドソケットのゼロコピー経路を維持。

## 受け入れ条件

- ホットパスに `libc::splice` の同期呼び出しが残らない（ripgrep で検証）。
- HTTP/1.1・kTLS パススルー・大容量ボディ転送の E2E が通過。
- スループットが退行しない（ベンチで確認）。

## 依存・リスク

- io_uring splice の挙動（短絡転送、SPLICE_F_* フラグ）とパイプ容量の扱いに注意。
- 転送ループの状態機械が複雑化。ホットパス絶対規則上は許容（むしろ必須）。
