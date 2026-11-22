# F-317: HttpArena の infrastructure で veil の点を上げる（親チケット）

**状態: 未着手（2026-11-22 起票。調査とチケットまで。改修は人の依頼の後）**

利用者の依頼（2026-11-22）: HttpArena の infrastructure の veil の結果を確認し、性能改善案を作る。ボトルネックは Proxy-Wasm と見ているので
Proxy-Wasm の改善、または veil に大きな変更を加えずに結果を良く見せる方法を考える。

## 点の付け方

- infrastructure は 9 プロファイル（baseline・pipelined・limited-conn・json-tls・static-tls・baseline-h2・static-h2・baseline-h3・static-h3）。
  プロファイルごとに接続数の rps の平均を infrastructure の中の 1 位で割って 1000 倍し、足す（満点 9000）。h2c・gRPC 等は参考で点に入らない。
- 設定の制限は無い。ハンドラはサーバが読み込むモジュールであればよい（veil は Proxy-Wasm のフィルタ、nginx は C のモジュール）。

## 今の結果（HttpArena の `site/data/results`、v0.10.1 のイメージ）

| エントリ | baseline | pipelined | limited | json-tls | static-tls | base-h2 | static-h2 | base-h3 | static-h3 | 計 |
|---|---|---|---|---|---|---|---|---|---|---|
| agensio | 977 | 558 | 972 | 1000 | 1000 | 912 | 1000 | 1000 | 930 | 8350 |
| fit | 650 | 1000 | 593 | 466 | 730 | 412 | 321 | 449 | 1000 | 5620 |
| nginx | 902 | 454 | 990 | 766 | 605 | 212 | 534 | 562 | 451 | 5476 |
| h2o | 1000 | 556 | 1000 | 716 | 0 | 1000 | 236 | 0 | 0 | 4508 |
| varnish | 212 | 255 | 172 | 0 | 262 | 15 | 97 | 0 | 0 | 1014 |
| caddy | 119 | 86 | 90 | 188 | 134 | 25 | 77 | 42 | 28 | 786 |
| **veil** | 87 | 67 | 119 | 295 | 81 | 35 | 10 | 61 | 17 | **771** |
| vinyl-cache | 212 | 256 | 158 | 0 | 0 | 0 | 0 | 0 | 0 | 626 |

veil の rps: baseline 318k・pipelined 597k・limited 315k・json-tls 505k・static-tls 97k・baseline-h2 550k・static-h2 14k・baseline-h3 526k・
static-h3 14k（1 位は 3.6M・8.9M・2.7M・1.7M・1.2M・15.9M・1.5M・8.6M・0.8M）。同じ機械の veil-api（ライブラリのハンドラ）は
2.5M・3.4M・2.0M・1.2M・0.94M・5.3M・1.0M・2.6M・0.29M。

## 原因（調査の結果）

1. **静的配信の 3 プロファイル（static-tls・static-h2・static-h3）は Proxy-Wasm と関係ない設定の既定値の不具合**:
   エントリに `[performance]` が無いと open_file_cache の有効期間・最大件数が 0 になり（[B-495](../bugs/B-495-performance-section-default-zero-ofc.md)）、
   キャッシュに一度も当たらない。HTTP/2・HTTP/3 は本体を入れ直すたびに圧縮結果も捨てる（[B-496](../bugs/B-496-content-cache-reinsert-drops-compressed.md)）ので、
   要求ごとに br で圧縮し直す（1 要求 1.2〜1.4 ms の CPU）。CPU は 11〜19 コアしか使っていない。設定で回避でき、ノート PC で
   h2c の静的配信は 710 → 43,216 rps（[F-320](F-320-httparena-veil-entry-config.md)）。
2. **Wasm を通る 6 プロファイル（baseline・limited-conn・json-tls・baseline-h2・baseline-h3・pipelined）は Proxy-Wasm が律速**:
   どのプロトコルでも 0.3〜0.6M rps で止まり、1 要求の CPU が veil-api の 4〜8 倍（baseline 195 µs 対 26 µs）。ノート PC の Wasm の
   上乗せ（約 25 µs）の 4〜7 倍に膨らんでいて、コア数で費用が増える形。要求ごとに wasmtime の型レジストリのロックと全ワーカー
   共有の `Arc` の参照カウントへ数十回書いているのが本命（[F-318](F-318-wasm-per-request-shared-writes.md)）。単発の費用は
   [F-319](F-319-wasm-per-request-fixed-cost.md)。
3. **pipelined** は `/pipeline`（本文 `ok`）までフィルタで答えている。設定の固定応答で返せる（F-320）。
4. **HTTP/2 の p99 が約 5 秒**（veil・veil-api とも。[B-497](../bugs/B-497-h2-p99-stuck-at-run-length.md)）。点には入らないが結果の表に載り、
   止まったストリームがスループットも下げている可能性がある。
5. 範囲外の差（チケットは作っていない）: Wasm の無い veil-api でも、HTTP/2・HTTP/3 の 1 要求の CPU（10〜14 µs）が h2o・agensio
   （1.3〜2 µs）の 5〜10 倍。baseline-h2・baseline-h3 の 1 位（15.9M・8.6M）に近づくにはこの差を詰める必要がある。F-318 の伸び方の試験を
   Wasm の無い経路にも当てて、共有の書き込みがあるかを見るのが最初の手。

## 試算（infrastructure の総合点。他のエントリは今の値のまま）

| 段階 | 前提 | 点 | 順位 |
|---|---|---|---|
| 今 | — | 771 | 7 / 8 |
| A | F-320 の 1〜2（静的配信が veil-api 並み: 937k・1.0M・289k） | 約 2,480 | 5 |
| B | A + `/pipeline` を固定応答（3.4M） | 約 2,800 | 5 |
| C | B + F-318（Wasm の上乗せがノート PC 並みの 25 µs: baseline 1.2M・limited 1.07M・json-tls 0.74M・base-h2 1.7M・base-h3 1.0M） | 約 3,590 | 5 |

- 試算の道具: `docs/artifacts/benchmark/httparena-perf/scripts/infra_score.py`（引数で各プロファイルの rps を差し替える）。
- h2o（4,508）・nginx（5,476）を超えるには、C に加えて HTTP/2・HTTP/3 の要求あたりの費用（5）が要る。

## 進め方（提案）

1. **F-320 の 1〜3（設定だけ）**: veil 本体を変えずに A〜B。v0.11.0 のイメージで事前圧縮も使える。提出は人の確認の後。
2. **B-495・B-496**（小さい修正）: 設定を書かない利用者にも効く。次の版に入れる。
3. **F-318**: Wasm の 5〜6 プロファイルの本命。最初に伸び方の試験で原因を確かめてから直す。
4. **F-319**: 単発の費用。F-318 の後に測ってから。
5. **B-497**: 再現から。
6. 次の版の公開（タグは人の判断）と HttpArena の再提出（人の確認）。

## 計測の環境と記録

- HttpArena の結果: 公開リポジトリの `site/data/results/*.json`（取得時のコミットは `docs/artifacts/benchmark/httparena-perf/httparena-results/REPO_COMMIT.txt`）。
  計測は 64 スレッド（cpuset 0-31,64-95）・5 秒・`--network host --security-opt seccomp=unconfined`。負荷は gcannon（h1）・h2load（h2・h3）・wrk（TLS）。
- 手元: ノート PC（4 CPU、共有の機械で ±5% 程度揺れる）、veil-load（h1・h2c）、サーバの CPU は `/proc/<pid>/task/*/schedstat`。
  スクリプト・設定・結果は `docs/artifacts/benchmark/httparena-perf/`（git 管理外）。

## 子チケット

- [B-495](../bugs/B-495-performance-section-default-zero-ofc.md)・[B-496](../bugs/B-496-content-cache-reinsert-drops-compressed.md)・
  [B-497](../bugs/B-497-h2-p99-stuck-at-run-length.md)・[F-318](F-318-wasm-per-request-shared-writes.md)・
  [F-319](F-319-wasm-per-request-fixed-cost.md)・[F-320](F-320-httparena-veil-entry-config.md)

## 関連

- F-269（HttpArena への提出）・F-294（事前圧縮）・B-175（Wasm のインスタンスの再利用）
