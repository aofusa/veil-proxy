# B-114: BSD の perf ハーネス（run_perf_bsd.sh）が全計測 0 rps・veil 起動失敗になる

**状態: 完了（feat/f184-native-system-allocators）**

## 事象

F-184 の性能比較で `tools/perf/bsd/run_perf_bsd.sh` を FreeBSD 14.3 aarch64 の VM で実行すると、2 つの問題が重なって
計測できなかった。

1. veil が全シナリオで起動しない（`[perf] ERROR: veil(https) 起動失敗`）。veil のログ:
   `capsicum: capability mode requested but h2c requires an extra listener; aborting startup`
2. 1 を回避しても、veil・nginx とも全シナリオ 0 rps・エラー 0 で「完了」する。負荷ツールのログ:
   `timeout: exec(pinned): No such file or directory`

## 原因

1. ハーネスの構成は `capsicum_capability_mode = true` と `allow_security_failures = false` を指定し、h2c の追加リスナー
   も持つ。h2c があると capability mode に入れないため、F-181（fail-closed）以降は起動を中止する。F-181 前は警告を出して
   capsicum の権限制限モードで起動しており、これまでの計測もそのモードで動いていた。
2. F-176 の共通化で負荷生成を `run_timeout <秒> pinned <cpus> wrk ...` と書いていたが、`timeout(1)` は外部コマンドしか
   実行できず、シェル関数の `pinned` を exec できない。負荷ツールが 1 本も走らないまま 0 rps を記録していた
   （`ab_content_cache.sh` も同じ）。

## 改修

1. 構成を `allow_security_failures = true` にし、従来どおり権限制限モードで続行させる（コメントに理由を記載）。
2. `lib.sh` に `run_timeout_pinned <秒> <cpus> <cmd...>`（CPU を固定する側から外部コマンドの timeout を起動）を追加し、
   `run_perf_bsd.sh` の 3 か所と `ab_content_cache.sh` の 3 か所を置き換えた。

修正後、FreeBSD VM で全 8 シナリオが veil・nginx とも計測できることを確認した。
