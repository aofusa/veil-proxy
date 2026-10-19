# B-153: BSD の性能の計測が nginx を固定せず、上流を共有し、順の偏りがあった

**状態: 完了（feat/v080-release、2026-10-18）**

## 症状

`tools/perf/bsd/run_perf_bsd.sh` は説明では「veil・nginx とも同じ 2 コアに固定する」としていたが、nginx を固定していなかった。
proxy・L4 の上流は計測対象と同じ nginx のプロセスで、nginx の計測だけ中継と上流が同じイベントループで済んだ。毎回 nginx を
先に測っており、VM ではラウンドの中で後に測るほど遅いので veil に不利だった（l4_tcp 3B: 固定の順で 0.85、交互で 1.04〜1.13）。
起動直後の veil を測って 0 rps になることもあった。

## 修正

nginx を `SRV_CPUS` に固定し、上流を別プロセス（`nginx-up.conf`、`UP_CPUS`）にし、反復ごとに順を入れ替え、HTTP の応答を
待ってから測る。`syscalls_per_req.sh`・`profile_cpu.sh` も同じ固定にした。

## 関連

F-240、F-145・F-176（BSD の計測）。
