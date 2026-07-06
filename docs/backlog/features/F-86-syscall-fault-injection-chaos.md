# F-86: OS/Syscall レイヤーのフォールトインジェクション（syscall_chaos）

出典: [container_security_review_report.md](../../artifacts/container_security_review_report.md) 提案3。親: [F-53](F-53-chaos-engineering-expansion.md)。関連: [F-68](F-68-resource-exhaustion-tests.md)（リソース枯渇）・[B-24](../bugs/B-24-sq-full-future-hang.md)（SQ 満杯ハング）。

## 目的

Toxiproxy/Pumba はネットワークレイヤーの障害注入であり、io_uring 自体のシステムコール
レベルのエラーハンドリングはテストできていない。`io_uring_enter(2)` 等の syscall 呼び出しに
人為的なエラー（`EBUSY`, `ENOMEM`, `EFAULT`, `EINTR` 等）を注入し、リソース枯渇・SQ リング
溢れ時にプロキシが安全に（panic・メモリ破損・永久ハングせずに）リクエストを破棄・リトライ
できるかを検証する。

## 実装済み（2026-07-06）

- `tools/container_security/chaos/syscall_chaos.sh` を新設（既定 SKIP、`SKIP_SYSCALL_CHAOS=1`）。
  - Veil イメージは distroless 相当で strace を含まないため、Veil バイナリを取り出して
    `debian:bookworm-slim` + `strace` の専用実行イメージ（`veil-sec-strace:local`）を
    ビルドし、そこで `strace -f -e inject=<syscall>:error=<E>:when=<first>+<step>` により
    io_uring_enter / io_uring_setup に人為エラーを注入して Veil を起動する。
  - `--cap-add SYS_PTRACE --security-opt seccomp=unconfined` の専用コンテナで、suite 本体とは
    分離して起動。注入中に curl 連打で負荷をかけ、`panic` / `RUST_BACKTRACE` / `segfault` /
    `SIGSEGV` / `SIGABRT` の有無を検査。exit 0 は graceful-exit（fault で進行不能なら安全に
    シャットダウンするのが正しい挙動）、running なら回復プローブで健全性を再確認する。
  - 注入対象: `io_uring_enter:error={EBUSY,ENOMEM,EINTR}` / `io_uring_setup:error=EFAULT`。
    起動時の ring 初期化を避けるため注入開始オフセット（`SC_WHEN_FIRST=40`）を既定化。
  - `run.sh` フェーズ 3 群 + `SKIP_SYSCALL_CHAOS`/`SYSCALL_CHAOS_BLOCKING` を配線。
  - レポート `syscall_chaos_report.txt`。
- 検証結果: `io_uring_enter:error=EBUSY` 注入で **panic/segfault なし・exit 0 の graceful-exit**
  を確認（B-24 で修正済みの SQ 満杯ハングも再発せず）。docker デーモンが `/tmp` を参照
  できない環境向けにビルドコンテキストを `results/` 配下へ置く回避も実装。

## 改修内容（当初案）

- `tools/container_security/chaos/syscall_chaos.sh` を新設（既定 SKIP、`SKIP_SYSCALL_CHAOS=1`）。
  `strace -f -e inject=io_uring_enter:error=EBUSY:when=N+step` 方式で Veil プロセスへ
  ランダム/周期的にエラーを注入しつつ HTTP 負荷を与え、以下を検証:
  - プロセスが panic / abort / segfault しない。
  - 注入停止後にヘルスチェック・通常応答が回復する（自己修復）。
- 注入対象エラー: `EBUSY` / `ENOMEM` / `EFAULT` / `EINTR` をパラメタライズ。
- strace には `CAP_SYS_PTRACE` が必要なため、専用の実行プロファイル（`--cap-add SYS_PTRACE`、
  seccomp との両立）をスクリプト内で構成し、通常フェーズの Veil コンテナとは分離して起動する。
- 結果を `syscall_chaos_report.txt` として `lib/report.sh` の集約対象へ配線。

## 受け入れ条件

- [x] `SKIP_SYSCALL_CHAOS=0 ./tools/container_security/run.sh` でフェーズが実行され、
      注入中も panic/segfault せず（EBUSY 注入で graceful-exit を確認）。
- [x] 発見された不具合はバグチケット化し修正すること（本実装では panic 系の新規不具合なし。
      io_uring_enter への持続的 EBUSY/EINTR 注入でも安全にシャットダウンする挙動を確認）。
- [x] README（container_security）へフェーズ・環境変数を追記すること。

## 依存・リスク

- strace 注入は ptrace ベースで大幅減速する（スループット検証ではなく生存性検証に限定）。
- コンテナ環境の ptrace 制約（Yama 等）でスキップされる場合は理由をレポートに記録する。
