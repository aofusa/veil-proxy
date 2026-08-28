# docs/perf — パフォーマンス計測サマリ

Veil の HTTP/1.1・HTTP/2・HTTP/3・gRPC・WebSocket・L4 のスループット／レイテンシ／
CPU・メモリ使用量を、`nginx:alpine` を基準に **同一 Docker ネットワーク上のコンテナ間通信**
で計測した結果のサマリ。

- 計測ハーネス: [`tools/perf/`](../../tools/perf/)（`gen_configs.sh` で構成生成 /
  `run_perf.sh` で反復計測 / `analyze_results.sh` で median±stdev 集計）。
- **本ディレクトリには「最新のフルスイート結果のみ」を記載する。** 過去の計測結果は
  git 履歴で辿れるため本ファイルには残さない（計測方針の教訓だけは末尾に蓄積する）。
- 生データは [`results_raw.tsv`](results_raw.tsv)。
  `bash tools/perf/analyze_results.sh docs/perf/results_raw.tsv` で再集計できる。
  行順は **nginx ベースライン → veil_glibc 各構成 → veil_container 各構成**（F-118）。
- **FreeBSD ネイティブ計測の生データは [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv)**
  （Docker が使えない FreeBSD 用の別ハーネス `tools/perf/freebsd/` の出力）。

---

## 2026-08-27 フルスイート（B-76 / F-169 / B-77 / B-78 / B-79 適用後）

- コミット: `perf/f169-benchmark-and-packaging`（B-76 CBPF 修正・F-169 圧縮キャッシュ・
  B-77 HTTP/3 静的圧縮・依存更新 anyhow/crossbeam-epoch/lru/h2/quinn-proto を含む）
- 実行: `BUILDS='glibc container' ITERATIONS=3 bash tools/perf/run_perf.sh`
- イメージ: `veil:glibc`（`CARGO_FEATURES=full`、io_uring）/
  `veil:container`（`CARGO_FEATURES=full-container`、epoll reactor）。
  **同一コミット・同一日ビルド**。比較対象は `nginx:alpine`（`access_log off`、
  h2c サーバブロックに `open_file_cache`）。
- **822 計測すべてで Non-2xx = 0、NA 行ゼロ。**

### 代表構成（Req/s 中央値）

| 構成 | プロトコル | nginx | veil glibc | veil container | 対 nginx（glibc） |
|---|---|---|---|---|---|
| `h2c_file`（3B 静的・h2c） | h2c | 10,572 | **25,178** | 25,322 | **2.38×** |
| `h2c_proxy`（54KB 中継・h2c） | h2c | 6,263 | **10,982** | 10,931 | **1.75×** |
| `h2_1_ktls_0_lb_kernel_ofc_1` | HTTP/1.1 | 6,534 | **9,238** | 9,281 | **1.41×** |
| 同上 | HTTP/2 | 5,971 | **7,373** | 7,342 | **1.23×** |
| `h2_1_ktls_0_lb_cbpf_ofc_1` | HTTP/1.1 | 6,534 | **9,308** | 9,159 | **1.42×** |
| `h2_0_ktls_0_lb_cbpf_ofc_0` | HTTP/1.1 | 6,534 | **8,200** | 8,175 | **1.26×** |
| `h2_0_feat_l4`（L4 平文 9080） | L4 | 6,534 | **13,752** | 12,770 | **2.10×** |
| `h2_1_feat_proxy` | HTTP/1.1 | 6,534※ | 6,422 | 6,431 | 0.98※ |
| `h3_file_metrics` | HTTP/3 | — | 1,874 | 1,952 | — |
| `h3_proxy` | HTTP/3 | — | 1,577 | 1,652 | — |
| `grpc_h2_metrics` | gRPC(k6) | — | 4,486 | 4,513 | — |
| `grpc_h3_metrics` | gRPC over HTTP/3 | — | 7,851 | 8,216 | — |
| `h2_1_feat_websocket` | WebSocket | — | 3,180 | 4,814 | — |

※ nginx ベースラインは TLS **静的配信**であり逆プロキシではない（F-118 の方針）。
`feat_proxy` 行の対 nginx は同条件比較ではない。**プロキシ同士の同条件比較は `h2c_proxy` のみ**
（1.75×）。

### B-76: CBPF 振り分けの修正で cbpf 構成が 1.27〜1.93 倍に

`reuseport_balancing = "cbpf"` は cBPF プログラムが常に 0 を返しており、
**全接続がワーカー 0 に固定**されていた（詳細は
[`B-76`](../backlog/bugs/B-76-reuseport-cbpf-pins-worker0.md)）。修正前は veil の全構成中
**cbpf 構成だけが nginx に負けていた**（0.70×）。

| 構成 | プロトコル | 修正前（08-26） | 修正後 | 比 |
|---|---|---|---|---|
| `h2_0_ktls_0_lb_cbpf_ofc_0` | HTTP/1.1 | 4,763 | **8,200** | **1.72×** |
| `h2_0_ktls_0_lb_cbpf_ofc_1` | HTTP/1.1 | 5,788 | **9,231** | **1.59×** |
| `h2_1_ktls_1_lb_cbpf_ofc_0` | HTTP/1.1 | 3,458 | **6,683** | **1.93×** |
| `h2_1_ktls_0_lb_cbpf_ofc_0` | HTTP/2 | 5,534 | **7,092** | **1.28×** |

修正後は cbpf 構成が kernel 構成とほぼ同値になる（どちらも 4 タプルをハッシュするため）。
これが「4 ワーカー全部が接続を受理するようになった」ことの直接の証拠である。

### F-169: 静的配信 + 圧縮が対 nginx 2.9 倍に

B-76 修正後、**対 nginx で明確に劣後する構成は圧縮系だけ**（0.27〜0.41×）になった。
[`F-169`](../backlog/features/F-169-static-compressed-variant-cache.md) で
(A) 静的配信の圧縮結果キャッシュ と (B) zstd 圧縮コンテキストの使い回し を実装した。

| 構成 | プロトコル | 静的キャッシュ無効 | **有効（推奨構成）** | nginx | 対 nginx |
|---|---|---|---|---|---|
| `h2_1_feat_compression` | HTTP/2 | 3,756 | **17,887** | 6,128 | **2.92×** |
| `h3_file_compression` | HTTP/2 | 4,079 | **17,817** | 6,128 | **2.91×** |
| `h2_1_feat_compression` | HTTP/1.1 | 8,168 | **9,593** | 6,633 | **1.45×** |

**`*_compression_cached` 構成は F-169 で新設した**（`static_file_cache` +
`open_file_cache` を有効にした推奨構成）。既存の `*_compression` 構成は
キャッシュ無効時のコストを測るためそのまま残してある。
どちらも 1 レスポンスあたり **16,467 / 16,464 バイト**（54,576B の zstd 圧縮後）で
**出力は同一**であり、差はキャッシュヒットで圧縮処理そのものが消えたぶんである。

**寄与の内訳（当初の仮説とは逆だった）**: 既存 compression 構成での +64〜76% は
**すべて (B) の zstd コンテキスト使い回し**によるもので、(A) のキャッシュは
`static_file_cache` 無効のため一度も動いていなかった。決定的な証拠は
**構造的にキャッシュが効かないプロキシ構成が同率（+65〜73%）で改善したこと**である。

### 計測のばらつきについて（重要）

`h2_1_proxy_compression` の HTTP/1.1 行は、**同一イメージ・同一設定でもラン間で
14.6% 振れる**（ラン内は 1% 未満）。

| 独立ラン | 3 反復の値 | 中央値 |
|---|---|---|
| A | 1921.5 / 1930.9 / 1926.0 | 1,926 |
| B | 1683.6 / 1696.1 / 1678.5 | 1,684 |
| C | 1912.8 / 1934.5 / 1930.2 | 1,930 |

**この構成の単発比較で退行を判断してはならない。** 本スイートで 0.95× を下回った
6 構成（`h2_1_proxy_compression` / `h2_1_feat_buffering` / `grpc_h3*`）は、
いずれも再計測で元の水準へ戻るか、ビルド間で方向が一致しない（片方が上がり片方が下がる）
ことを確認しており、**コード起因の退行は 1 件も無い**。

### 退行確認（2026-08-26 フルスイートとの比較）

比較可能な **225 の (構成, プロトコル) ペアすべてで 0.95× を下回る退行は無し**
（上記のばらつき検証で除外した 6 件を除く）。

## FreeBSD ネイティブ計測（本セッションでは未取得）

`tools/perf/` 本体は Docker 前提のため FreeBSD では動かない。専用ハーネス
[`tools/perf/freebsd/run_perf_freebsd.sh`](../../tools/perf/freebsd/run_perf_freebsd.sh) を
**ゲスト内 loopback で veil と nginx を同条件**（同じ 2 コアへ cpuset 固定・
負荷生成は残り 2 コア・双方アクセスログ off・双方 `open_file_cache` 有効）で実行する。

**本セッション（2026-08-27）では FreeBSD の再計測を完了できていない。**
計測用の macOS ホスト（QEMU + HVF で FreeBSD 14.3 aarch64 VM を動かしている）が
セッション中に繰り返しネットワークごと落ち、最終的に到達不能のままとなったため。
VM 内のビルド（HEAD 相当）までは完了している。生データ
[`freebsd_results_raw.tsv`](freebsd_results_raw.tsv) は **2026-08-07〜08 時点のもの**で、
本セッションの変更（B-76 / F-169 / B-77）は反映されていない。

なお本セッションの変更のうち **B-76（CBPF）は Linux 専用**（`SO_ATTACH_REUSEPORT_CBPF`）で
FreeBSD には影響しない。**F-169 / B-77 は FreeBSD にも効く**（圧縮結果キャッシュと
HTTP/3 静的圧縮）ため、再計測時は `h2_file_tls` / `h2c_file_plain` に加えて
圧縮構成を確認すること。

---

## 計測条件

- ホスト: 4 コア Linux（co-tenant あり）。**クライアント・veil・上流が同一マシンを共有する**
  ため、veil 単体の改善は rps に鈍く出る。改善幅を正確に見たい場合は
  `tools/perf/h2c_proxy_lab.sh cpuab`（veil の CPU/req 交互 A/B）を使う。
- 負荷: HTTP/1.1 = wrk `-t4 -c100 -d10s` / HTTP/2・HTTP/3 = h2load `-n 30000 -c100 -m10` /
  gRPC = k6 50VU×10s / gRPC over HTTP/3 = QUIC 対応 h2load + gRPC unary ワイヤ形式（F-167）/
  WebSocket = k6 / L4 = wrk（平文 9080）
- 圧縮構成のみ `Accept-Encoding: gzip, br, zstd` を付与する（付けないと圧縮経路を通らない）。
- 各 (config, proto) を warmup 後 3 反復、median±stdev 集計。Errors は Non-2xx。
- kTLS はコンテナ（veth）と相性が悪いため feat 系構成では無効（直交表の ktls 因子でのみ計測）。

---

## 教訓（計測方針に反映済み）

- **コンテナ（veth/bridge）では kTLS が不利**。feat 系構成は kTLS 既定オフ。
- **ホスト負荷（co-tenant のビルド等）が計測ノイズの支配的要因**。静穏ウィンドウ
  （1 分 loadavg 目安 < 1.5）を確認してから計測し、比較は必ず**同日・同一環境の A/B**
  （nginx 併走で環境ノイズを正規化）で行う。
- **h2load の `failed`（ストリームエラー）は Non-2xx に計上されない**。Errors=0 でも
  異常低スループット時は h2load の `requests:` 行とサーバ warn ログを確認する
  （B-43・B-46 の教訓）。
- **Docker seccomp 許可リストは使用 syscall の追加に追随させる**（F-115 の教訓）。
- **h2load は既定 1 スレッドでクライアント律速になり得る**。HTTP/2 で 2800 req/s 級以上を
  計測する際は `H2_ARGS='-n 60000 -c100 -m10 -t4'` を併用する（F-116 の教訓）。
- **高並行の多重化計測は fd 上限・接続チャーンの検出器になる**。0 req/s 近傍や反復劣化を
  見たら、サーバの `Too many open files` / `Backend connect error` ログと
  `/proc/net/tcp` の状態分布を確認する（B-44/B-45 の教訓）。
- **git worktree から tools/perf を実行する場合、git 管理外の生成物
  （`docker/assets/ssl/*.pem` 等）を本体ツリーからコピーする**（F-116 A/B の教訓）。
- **FreeBSD の kTLS は大きな応答で不利**（`sysctl kern.ipc.tls.enable=1` でカーネル側を
  有効にしたうえで veil の `ktls_enabled = true` にした場合）。software kTLS が TLS
  レコードごとにカーネルワーカースレッドへディスパッチするため、54KB 応答で
  **中央値 26% 低下**し 1.2 GB/s で直列化により張り付く（上記「FreeBSD の kTLS は
  大きな応答で不利」節）。計測前に `kern.ipc.tls.stats.sw.gcm` が増えているかで実際に kTLS
  セッションが張られたかを確認する（F-145 の教訓）。
- **QEMU/HVF 上の VM は連続計測でスループットが単調に劣化する**（実測: 同一バイナリで
  18,217 → 15,153 → 12,845 rps）。A/B で「先に走った方が有利」という順序バイアスが
  効果量と同オーダーになるため、**交互かつ短時間サンプルを多数取る**こと。本環境では
  5〜10% の差は判定できない（F-145 の教訓）。
- **reactor バックエンド（BSD/macOS/`full-container`）は Linux 既定ビルドで 1 行も
  コンパイルされない**。`src/runtime/reactor/` を変更したら
  `VEIL_E2E_FEATURES="full,epoll" ./tests/e2e_setup.sh test` を必ず回す。F-145 では
  「最初の 1 リクエストでサーバがハングする」変更が単体 816 / 統合 53 / E2E 541 件の
  **全テストを通過**した（AGENTS.md に明記済み）。
- **「未対応ボトルネック」の原因仮説は、実装前に必ず計測で確定させる**（F-151 の教訓）。
  `docs/perf` に書いていた「HTTP/3 は QUIC の暗号処理・輻輳制御が原因と見られる」という
  仮説は、`cc_algorithm`/`pacing` の A/B で**有意差なし**として否定された。真因は
  メインループの O(N_conn) 固定費で、**`mmsg_batch_size` を変えて「ループ回数だけ」を
  動かす実験**（トラフィック量・暗号処理量・輻輳制御は不変）で 3.2 倍の差が出たことで
  確定した。**「何を変えたら何が動くか」を 1 変数に絞った実験を先に作ること。**
- **接続数を振って測ると律速の性質が分かる**（F-151 の教訓）。接続数に依存しない頭打ちは
  「1 リクエストあたりのコスト」、接続数に比例して悪化するなら「接続数あたりの固定費」。
  さらに**負荷生成側を 2 プロセスに割って合計が変わらないこと**を確認すると、
  クライアント律速を確実に排除できる。
- **計測ハーネスは「0 rps を記録して正常終了する」失敗モードを持つ**（2026-08-11 の教訓）。
  FreeBSD の HTTP/3 計測は、(1) `h3load` が密着形短オプション（`-t2`）を受理せず即終了、
  (2) 固定リクエスト数を外側 `timeout` が撃ち殺して集計行が出ない、の 2 件により
  **veil・nginx とも 0 rps** を記録し続けていた。どちらもハーネスはエラー終了しない。
  **新しい環境で計測したら、まず 0 / NA の行が無いかを確認すること。**
  負荷生成は可能なかぎり**固定リクエスト数ではなく時間で区切る**（マシン速度に依存しない）。
- **計測は必ず静穏ホストで**。並行ビルド中の E2E は 415 passed / 127 failed になり、
  静穏時は 541 passed / 1 failed だった（所要 822 秒 → 88 秒）。大量失敗を見たら
  まず loadavg を疑う。

- **同一イメージでもラン間で 15% 振れる構成がある（2026-08-27）。** `h2_1_proxy_compression`
  の HTTP/1.1 は、ラン内の 3 反復は 1% 未満に収まるのに、**独立ランの中央値が
  1,684〜1,930（14.6%）** ばらついた。**ラン内のばらつきの小ささを、その構成の
  再現性の高さと取り違えてはならない。** 退行判定は必ず「再計測して戻るか」
  「ビルド間で方向が一致するか」で行う。
- **計測構成がその機能の有効化条件を満たしているか先に確認する（F-169）。**
  既存 compression 構成は `static_file_cache` を有効にしておらず、F-169 の圧縮結果
  キャッシュは**有効化条件を満たさず一度も動かなかった**。効いていたのは別施策
  （zstd コンテキストの使い回し）で、切り分けができたのは
  **構造的にキャッシュが効かないプロキシ構成が同じ幅で改善した**ことに気づいたためである。
  F-157（メタデータ／本体キャッシュの片側だけ有効化）と同じ罠の 3 度目。
- **ライブラリの「ワンショット API」はコンテキストを作り捨てている可能性を疑う（F-169）。**
  `zstd::encode_all` は呼び出しごとに圧縮ワークスペースを確保・初期化していた。
  ホットパスでは再利用可能なコンテキスト型（`zstd::bulk::Compressor` 等）を使う。
- **手元の docker run で挙動を再現するときは、ハーネスと同じマウント先を使う（2026-08-27）。**
  `tools/perf/run_perf.sh` は計測用設定を **`/etc/veil/conf.d/config.toml`** へマウントする。
  `/etc/veil/config.toml` へマウントするとイメージ同梱の既定設定（静的 File ルート）が
  生き残り、**まったく別の経路を測ってしまう**。さらに **snap 版 docker は
  `/tmp/claude-*` のようなホスト外パスを bind mount できず、黙って空ディレクトリを
  作る**（エラーにならない）。実際にこの 2 つが重なり「HTTP/1.1 は圧縮しない」という
  誤った結論を出しかけた（正しくは `content-encoding: zstd` で 54,576B → 17,224B）。
  **設定を差し替えた検証では、まず起動ログで意図した経路（Proxy か SendFile か）を確認する。**

## 再現手順

### FreeBSD ネイティブ計測

```bash
# 1) FreeBSD ゲストを起動してビルド（Apple Silicon macOS では HVF で実用速度）
tools/qemu/bsd-vm.sh freebsd aarch64 up
CARGO_FEATURES=full-freebsd-aarch64 tools/qemu/bsd-vm.sh freebsd aarch64 build

# 2) HTTP/3 クライアント（FreeBSD には QUIC 対応 h2load が無いため必須）
#    ゲスト内で:
cargo build --release --manifest-path tools/perf/h3load/Cargo.toml-aarch64

# 3) 計測（ゲスト内。nginx / wrk / nghttp2 が必要）
pkg install -y nginx nghttp2 wrk-luajit
sh tools/perf/freebsd/run_perf_freebsd.sh -r 3 -d 10                 # 54KB
sh tools/perf/freebsd/run_perf_freebsd.sh -r 3 -d 10 -p /small.html  # 3B

# ホスト（macOS）からは薄いラッパ経由でも実行できる
bash tools/perf/freebsd/vmrun.sh -r 3 -d 10
```

### Docker ベース計測（Linux）

```bash
docker build -f docker/Dockerfile.glibc -t veil:glibc --build-arg CARGO_FEATURES='full' .
docker build -f docker/Dockerfile.musl  -t veil:musl  --build-arg CARGO_FEATURES='full' .
docker build -t local/h2load-h3:latest tools/perf/h2load-http3   # HTTP/3 クライアント

bash tools/perf/gen_configs.sh
bash tools/perf/run_perf.sh                                      # 全構成スイート（~5 時間）
# scoped 計測の例:
CONFIG_GLOB='h2_1_feat_http3'             bash tools/perf/run_perf.sh   # HTTP/3 file
CONFIG_GLOB='h2_1_ktls_0_lb_kernel_ofc_1' bash tools/perf/run_perf.sh   # H1/H2 best
CONFIG_GLOB='grpc_*'                      bash tools/perf/run_perf.sh   # gRPC
CONFIG_GLOB='h2_0_feat_l4'                bash tools/perf/run_perf.sh   # L4

# B-46 リグレッション確認（修正前は 2xx ヘッダのみ・ボディ 0B で全 failed になる）
# h3_proxy_buffering 構成の veil に対して:
h2load --alpn-list=h3 -n 100 -c 10 -m10 https://<veil>:443/
```

