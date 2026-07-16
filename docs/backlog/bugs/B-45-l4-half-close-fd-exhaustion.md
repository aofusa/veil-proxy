# B-45: L4 プロキシの半クローズ未伝搬による fd 滞留 → EMFILE で全断

## 事象

F-118 の全構成 perf 計測（2026-07-15）で、L4 構成（`h2_0_feat_l4`、平文 9080 素通し）が
反復ごとに劣化し **3 回目で 0 req/s（完全停止）**（4971 → 1524 → 0）。glibc / musl 両方で再現。
veil ログに以下が大量発生:

```
WARN  [src/l4/proxy.rs] [L4:perf-l4] failed to connect to upstream ...: Too many open files (os error 24)
ERROR [src/l4/server.rs] [L4:perf-l4] accept error: Too many open files (os error 24)
```

## 再現手順

```bash
# perf ハーネスの h2_0_feat_l4 構成で veil + perf-backend(nginx) を起動し
wrk -t4 -c100 -d10s http://<veil>:9080/   # を連続 3 回実行
# → fd（nofile ソフトリミット 1024）を使い切り、3 回目に 0 req/s
# /proc/<pid>/fd はパイプ read/write 端のペアが大量に滞留
```

## 調査

- `bidirectional_forward` は `futures::join!` で **両方向の転送ループが終わるまで** 接続
  リソース（ソケット 2 + splice パイプ 2 = 4 fd）を保持する。
- `forward_direction_splice` は src の EOF（splice が 0 を返す）で自方向を抜けるが、
  **dst へ半クローズ（`shutdown(SHUT_WR)`）を伝搬しない**。このためクライアントが
  接続を閉じても上流（nginx keepalive）は FIN を受け取らず接続を維持し、
  u→c 方向は `readable()` でアイドルタイムアウトまで待機し続ける。
- 結果、クローズ済みコネクションが 1 本あたり 4 fd をアイドルタイムアウトまで滞留させ、
  wrk の接続チャーン（read エラー → 再接続の雪崩）と相まって nofile 1024 を突破。
  EMFILE 後は accept も upstream connect も失敗し全断する。

## 改修案（実施）

`forward_direction_splice` / `forward_direction`（ユーザースペースフォールバック）の
転送ループ離脱時に、**dst へ `shutdown(dst_fd, libc::SHUT_WR)` を発行して EOF を伝搬**する:

- src EOF（正常クローズ）時: dst へ SHUT_WR → 対向がクローズを検知して FIN を返し、
  逆方向ループも即座に EOF で終了 → セッション全 fd が即時解放される。
  片方向クローズ後も逆方向の転送は継続できる（TCP 半クローズ意味論を保持）。
- エラー・アイドルタイムアウト離脱時も同様に SHUT_WR を発行し、対向方向を解放する。
- `shutdown(2)` はノンブロッキング syscall（seccomp 許可済み・io_uring 新規オペコード不要）。

## 受け入れ条件

- [ ] wrk `-t4 -c100 -d10s` 連続 3 回でスループット非劣化・EMFILE なし。
- [ ] クライアントクローズ後にセッションの fd が即時解放される回帰テスト。
- [ ] 既存 L4 E2E（TLS パススルー含む）に退行なし。

## 関連

- F-18（L4 ストリームプロキシ）/ F-30（splice ゼロコピー）/ F-40（パイププール）
- 検出: F-118 perf 計測（`docs/artifacts/perf_reports/f118/`）
