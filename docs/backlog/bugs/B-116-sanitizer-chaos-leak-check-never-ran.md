# B-116: E2E sanitizer カオス（e2e_sanitizer_chaos.sh）の LeakSanitizer が一度も走っていなかった

**状態: 完了（feat/f185-memory-leak-check）**

## 事象

`tools/container_security/chaos/e2e_sanitizer_chaos.sh`（F-85）は ASAN ビルドの veil に負荷・接続チャーン・SIGHUP を
かけたあと SIGTERM を送り、「プロセス終了時の LeakSanitizer レポート」をログから探して「エラーなし」と報告していた。
F-185 で終了を待つようにすると、LeakSanitizer の検査は一度も走っていなかった。理由は次の 3 つが重なっていたこと。

1. SIGTERM で veil が終了しなかった（B-115。コンテナの PID 1 で SIGTERM が無視される）。スクリプトは TERM の
   3 秒後にログを見ていたので、終了時に出るはずの LeakSanitizer の出力が無いのを「リークなし」と判定していた。
2. B-115 を直して終了するようになると、LeakSanitizer が
   `LeakSanitizer has encountered a fatal error`（`does not work under ptrace`）で落ちた。LeakSanitizer は
   ptrace で全スレッドを止めてから走査するが、veil 自身の seccomp（ptrace を許可しない）・Landlock（ドメイン内からの
   ptrace を拒否）・特権降格（setuid 後は dumpable でなくなる）がそれぞれ妨げる。
3. コンテナの設定（`docker/assets/conf.d/config.toml`）は 3 つとも有効だった。

## 改修

- TERM のあと最大 `SAN_EXIT_WAIT`（既定 120 秒）終了を待ち、終了しなければ「未検証」で失敗、終了コードが 0 以外
  （LeakSanitizer はリークがあると非 0）でも失敗にした。
- sanitizer 実行用の設定を生成し、seccomp・Landlock・特権降格を外す（コンテナの seccomp は従来どおり unconfined）。

## 確認

- `LSAN_OPTIONS=verbosity=1` で LeakSanitizer が全スレッドを走査すること（`Processing thread`）を確認した。
- 負荷 60 秒（HTTPS・HTTP の並列 curl のチャーン）+ SIGHUP 3 回 + 追加チャーンのあと SIGTERM で終了コード 0、
  LeakSanitizer の報告なし（`ktls,http2,cache,buffering,compression,metrics,websocket,rate-limit,access-log,l4-proxy`）。
