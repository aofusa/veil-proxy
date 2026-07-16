# B-44: HTTP/2 プロキシのバックエンド接続チャーンによるポート枯渇（F-116 多重化で顕在化）

## 事象

F-118 の全構成 perf 計測（2026-07-15）で、HTTP/2 の Proxy 系構成
（`h2_1_feat_proxy` / `h2_1_proxy_*` / `h3_proxy*` の http2 プロトコル）が
**~590 req/s（前回 2026-07-13 計測の ~2150 req/s から −73%）**、平均レイテンシ ~1.5s、
反復 2 回目以降に **Non-2xx（502）が最大 4% 混入**する退行を検出。glibc / musl 両方で再現
（= 環境でなくコード起因）。HTTP/1.1 プロキシ・File 配信・gRPC・HTTP/3 は退行なし。

## 再現手順

```bash
# perf ハーネスの h2_1_feat_proxy 構成で veil + perf-backend(nginx) を起動し
h2load -n 30000 -c100 -m10 https://<veil>:443/   # を連続 3 回実行
# → 2 回目以降に 5xx が数百件、スループット ~600 req/s に低下
docker logs <veil> | grep 'Backend connect error'
# → "Cannot assign requested address (os error 99)" (EADDRNOTAVAIL) が多数
```

## 調査

- 5xx の正体は `h2_proxy_http` の `TcpStream::connect_str` が **EADDRNOTAVAIL** で失敗した
  502。エフェメラルポートの枯渇（TIME_WAIT 蓄積）が原因。
- F-116 のストリーム多重化により、1 コネクション同時 10 ストリーム × 100 コネクション =
  **同時 1000 リクエストがそれぞれバックエンド接続を取得**するようになった。
  バックエンド接続プール（`HTTP_POOL`、B-28）のホストあたりアイドル上限は
  `BACKEND_POOL_MAX_IDLE_PER_HOST = 8`（スレッドごと）のため、完了波のたびに
  上限超過分（スレッドあたり同時 ~250 に対し 8）が **クローズ → TIME_WAIT** となり、
  実質リクエストごとに新規 TCP 接続を張る状態に退化（チャーン ~97%）。
- F-116 以前は 1 コネクション直列処理（同時 ~100）で、put 直後に同一クライアント
  コネクションの次リクエストが get するため実質完全再利用となり顕在化しなかった。
- 計測後のバックエンド ESTABLISHED は 32 本のみ = 同時接続数ではなくチャーンが原因。

## 改修案（実施）

- `BACKEND_POOL_MAX_IDLE_PER_HOST` を **8 → 256** に引き上げ、多重化後の
  スレッドあたり同時ストリーム数（実測 ~250）を吸収する。
  - アイドル接続は `BACKEND_POOL_IDLE_TIMEOUT_SECS = 30` 秒で回収されるため、
    定常アイドル時の fd 保持は一時的（Envoy の upstream 接続上限既定 1024 と同水準）。
  - `[security] max_idle_connections_per_host` で従来どおり上書き可能（既定値のみ変更）。
  - 同じ既定値を共有する H2C プール（F-106、gRPC 中継）にも同様に効く。

## 追加調査（第2段: 2026-07-16）

プール上限 8 → 256 で定常時の再利用は回復（新規 connect 869/30000 リクエスト = 再利用 ~97%、
エラー 0・1934 req/s）したが、**コールドスタート/反復切り替え直後の接続ストーム**
（同時 ~1000 ストリームの一斉新規 connect）で依然 `EADDRNOTAVAIL` が数秒バーストする
（最大 ~2000 件/30000）。切り分け:

- TIME_WAIT はピーク 13・ESTABLISHED ~1000（port range 28k に遠い）→ ポート枯渇ではない。
- 素の `connect(2)` では同一条件（1000 同時、RST 一斉クローズ後の再ストーム含む）で再現しない
  （Python 実測エラー 0）→ カーネル一般の挙動ではなく **io_uring CONNECT 経路特有**。
  バックエンド nginx の SYN/accept キュー飽和時の失敗が `EADDRNOTAVAIL` として
  表面化していると推定。

→ 第2段修正: バックエンド connect の `EADDRNOTAVAIL` を **指数バックオフ（10/40/160ms、
最大 3 試行）のリトライで吸収**する（nginx `proxy_next_upstream` 相当の一時的失敗リトライ。
設計は `docs/artifacts/f118_perf_improvement_design.md` 追補参照）。

## 受け入れ条件

- [ ] h2load `-n 30000 -c100 -m10` 連続 3 回で Non-2xx=0・`Backend connect error` なし。
- [ ] HTTP/2 プロキシ構成のスループットが F-116 以前の水準（~2000 req/s 以上）へ回復。
- [ ] HTTP/1.1 プロキシ・File 配信・gRPC に退行なし。

## 関連

- F-116（HTTP/2 ストリーム多重化）/ B-28（バックエンド接続プーリング）/ F-106（H2C プール）
- 検出: F-118 perf 計測（`docs/artifacts/perf_reports/f118/`）
