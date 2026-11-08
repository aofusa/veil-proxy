# F-299: Python ハンドラの経路（GIL のレーン・境界の削減）

**状態: 完了（feat/v0120-perf、2026-11-07）**

親: [F-295](F-295-v0120-perf-harness.md)。[B-334](../bugs/B-334-python-thread-state-per-call.md)・[B-335](../bugs/B-335-access-log-writer-wake-per-request.md) を前提にする。

## 現状

`tools/api-bench` の veil + Python: plaintext 25.5k / json 22.6k（2 スレッド、offload=True）。目標は 50k 以上、できれば 70k。

aofish の WSL（改修前、Python 3.12、サーバ CPU 0-3 または 0-7、負荷 CPU 8-15、64 接続）:

| 構成 | plaintext | json |
|---|---|---|
| offload=True、2 スレッド | 22.6k | 21.2k |
| offload=True、4 スレッド | 30.4k | 29.7k |
| offload=False、1 スレッド | 11.5k | - |
| offload=False、2 スレッド | 15.8k | 14.6k |
| offload=False、4 スレッド | 12.5k | 11.2k |

Python 3.14 もほぼ同じ。スレッドを増やすと悪化するのは GIL の奪い合い（5 ms のスイッチ間隔と convoy）。p90 が 10 ms 以上。

## 原因の分解

1. 呼び出しごとに thread state を作って壊す（[B-334](../bugs/B-334-python-thread-state-per-call.md)）。
2. アクセスログのスレッドを要求ごとに起こす（[B-335](../bugs/B-335-access-log-writer-wake-per-request.md)）。
3. `offload=True` は呼び出しごとに: `to_worker(16)`（チャネルと `Arc`）、ジョブの `Box`、プールの `Mutex` + `Condvar` の `notify_one`
   （常に futex）、完了の eventfd。プールの全スレッドが GIL を奪い合う。
4. Python 側の `Route._make_call` の 2 段のラッパ（`call` → `invoke` → `func`）と `NativeRequest` の確保。

## 改修内容（設計）

### A. 永続の thread state（B-334）

スレッドごとに 1 回だけ `PyGILState_Ensure` して保持し、以後の attach は既存の thread state を再利用する。

### B. GIL のレーン（ハンドラプールのセッション）

ハンドラプールに「セッション」の差し込み口を足す（Rust 本体は Python を知らない）。

- ランナーは 1 本だけが GIL を持ったままキューを捌く（ジョブごとに GIL を手放さない）。ジョブは 1 つずつ取り出す。
- もう 1 本が **フォロワー** として `take_gil` で待つ。ハンドラがブロッキング（`time.sleep`・I/O）で GIL を手放すと、
  フォロワーがすぐ GIL を取ってキューを捌き、次のフォロワーを起こす。**GIL そのものが停止の検出器** になるので、
  ブロッキングなハンドラの並行性はこれまでどおり（プールのスレッド数まで）。
- ランナーが 2 本より多いときはジョブの合間に降りる。キューが空になったらランナーは眠る。
- 投入は「ランナーが足りないときだけ」起こすので、飽和時は futex が消える。
- C・Rust のブロッキングなハンドラは今までどおり並列（レーンは Python のルートだけ）。

### C. 完了の通知

完了はワーカーごとの受け箱へ積み、eventfd の書き込みをまとめる（ワーカーが起きているなら書かない）。
ランナー側で K 件ごと・キューが空になったときに通知する。

### D. 呼び出しごとの確保を減らす

`to_worker(16)` の代わりに 1 個の呼び出しスロット。ストリーミングのチャネルは `start_stream` のときに作る。

### E. Python の高速な呼び出し

Rust が利用者の関数を直接呼ぶ「計画」を持つ（引数なし・`request` のみ・パスの文字列のみのルート）。例外の場合だけ Python の遅い経路で
応答へ変換する。`NativeRequest` は使わないルートでは作らない。

### F. 本文

小さい本文は GIL の中で Rust の `Bytes` へコピーする（遅延 decref のミューテックスを避ける）。大きい本文はゼロコピーのまま。

### G. その他の経路

`async def`（`aio.rs`・`_aio.py`）と `offload=False` も同じ基盤の上で測って直す。

## 実装で設計から変えたこと

- レーンは本体のハンドラプールの「セッション」ではなく `crates/veil-python/src/lane.rs` の専用スレッドにした。本体が持つのは
  `api::bridge::BlockingExecutor`（ブロッキングな呼び出しの投入先の差し替え口）だけで、本体は Python を知らないまま。
- 完了の通知（C）は「ランナーが K 件ごと」でなく、ワーカーごとの受け口 `offload::Inbox` の「前回の取り出しの後の最初の 1 件だけ」fd を叩く形に
  した。ワーカーが取り出している間の完了は通知が要らない。
- E（Rust が利用者の関数を直接呼ぶ計画）は入れない。A〜D だけで目標を大きく超えたので、Python の呼び出しの規約を 2 通り持つ理由が無い。
- F（小さい本文の GIL 内コピー）も同じ理由で入れない。
- 要求は毎回新しい `contextvars.Context` の `run` で呼ぶ（thread state を残すと文脈もスレッドに残り、要求の間で漏れるため）。
  `threading.local` はスレッドに残る（ガイドに書いた）。

1 接続だけの負荷は改修前より 7〜11 µs 遅い（0.91×）。レーンの引き継ぎがスレッド間の受け渡しを 1 段増やすためで、4 接続で 1.57×、
512 接続で 3.52× になる。p50 が起動ごとの配置で 2 値に割れ、`Context`・FIFO・2 回目の通知は原因でないことまでは切り分けた。

## 計測

`python3 tools/perf/api_ab.py --scenario python`（`tools/api-bench/python/veil_app.py`。plaintext・json、2・4 スレッド、offload、async、
Python 3.12・3.14）。コア制限はサーバの CPU を固定して測る。`tools/api-bench/run_bench.py` にも目標の列を足す。

## 受け入れ条件

- 同じハードウェアで plaintext/json が 50k 以上（できれば 70k）。
- `offload=True` が既定のまま。ブロッキングなハンドラの並行性が落ちない（`time.sleep` のハンドラの試験）。
- FastAPI 風のデコレータの API を変えない。例外・検証エラー・`HTTPException` の挙動を変えない。
- `crates/veil-python` の試験と E2E が Python 3.9（abi3）〜3.14 で通る。

## 最終の A/B（v0.12.0 の候補 03cb80d0 と v0.11.0、aofish WSL、交互 A/B の中央値）

`tools/perf/api_ab.py --scenario python`（`veil_app.py`、アクセスログ有効、サーバ CPU 0-3、64 接続。3.12 は 5 ラウンド、3.14 は 3 ラウンド）。
「1 コア」はサーバを 1 つの物理コア（CPU 0-1）に固定した回。生データは `docs/perf/linux_api_ab_raw.tsv`。

| 行（2 スレッド） | Python 3.12 | Python 3.14 | 3.12、1 コア |
|---|---|---|---|
| offload、plaintext | 21,494 → **58,404**（2.72×）、p99 8.4 → 1.9 ms | 19,163 → 60,762（3.17×） | 35,418 → 58,207（1.64×） |
| offload、json | 20,745 → **57,198**（2.76×）、p99 8.9 → 2.1 ms | 20,795 → 57,171（2.75×） | 35,561 → 57,476（1.62×） |
| inline、plaintext | 17,872 → 61,897（3.46×） | 16,842 → 64,391（3.82×） | 21,127 → 62,161（2.94×） |
| inline、json | 17,759 → 57,163（3.22×） | 16,831 → 55,631（3.31×） | 20,350 → 53,839（2.65×） |
| `async def`、plaintext | 23,110 → 70,688（3.06×） | 23,366 → 70,777（3.03×） | 40,599 → 52,438（1.29×） |
| `async def`、json | 23,746 → 69,753（2.94×） | 23,355 → 71,159（3.05×） | 38,622 → 51,284（1.33×） |

4 スレッドの offload は 3.12 で 53,950 → 99,206（plaintext）・46,439 → 92,694（json）。CPU/要求は offload 2 スレッドで 90.3 → 33.4 µs。

`tools/api-bench/run_bench.py`（wrk、アクセスログ無し、サーバ CPU 0-1、FastAPI と同じ回。`docs/guide/library.md` の表、生データは
`docs/perf/linux_python_bench_raw.tsv`）では offload が 69,245 → 206,599（plaintext）・70,802 → 168,852（json）。

### 目標値の機械（i5-3320M、2026-11-08）

目標の「plaintext 25.5k / json 22.6k」は i5-3320M（2 コア 4 スレッド）の `run_bench.py`（`wrk`、アクセスログ無し、サーバ CPU 0-1・負荷 CPU 2-3）の
値。その機械に `wrk` が無いので、同じ配置を `tools/perf/veil-load`（2 スレッド、64 接続）で 5 ラウンド交互に測った（v0.11.0 `43c8045a` と
`c9689c23` の wheel、生データは `docs/perf/linux_python_i5_raw.tsv`、エラー 0）。

| 行（2 スレッド） | アクセスログ無し | アクセスログ有効 |
|---|---|---|
| offload、plaintext | 22,850 → 48,417（2.12×）、CPU/要求 78.2 → 41.0 µs | 20,556 → 45,681（2.22×） |
| offload、json | 20,373 → 46,069（2.26×）、CPU/要求 83.0 → 42.6 µs | 18,833 → 43,413（2.31×） |
| inline、plaintext | 24,909 → 45,344（1.82×） | 19,370 → 42,997（2.22×） |
| inline、json | 23,215 → 41,111（1.77×） | 18,296 → 39,171（2.14×） |
| `async def`、plaintext | 23,967 → 27,422（1.14×） | 23,286 → 28,039（1.20×） |
| `async def`、json | 22,283 → 24,667（1.11×） | 22,048 → 25,257（1.15×） |

`veil-load` の値は 50k に届かない。目標値から 50k に要る倍率（1.96× / 2.21×）は超えた。改修前の値が目標値の 0.90 倍なので、同じ比なら `wrk` で
54k / 51k に当たる（推定）。aofish で `run_bench.py` と同じ配置（2 スレッドを CPU 2-3）の `veil-load` は `wrk` の 0.77 倍（plaintext）・0.95 倍
（json）だった（`docs/perf/linux_loadgen_calib_raw.tsv`）。サーバの 2 つの CPU は使い切っていて、この機械の `veil-load` で 50k を超えるには
1 要求あたり 40 µs を切る必要がある。

試して入れなかったこと: レーンの走者が、列が空になっても GIL を握ったまま 20・50 µs 次の要求を待つ（眠りと起床を減らす）。同じバイナリの待ち 0 と
比べて ±2% の範囲で、速くならなかった（`docs/artifacts/v0120/lane-spin-experiment.diff`）。

**目標との対応。** aofish（同じハードウェアの前後）では、改修前が 21k 前後になる `api_ab.py` の Python のシナリオで offload・2 スレッドが
58k / 57k（50k 以上）、`async def` が 70k。`run_bench.py` では改修前の 2.4〜3.0 倍。目標値の機械では倍率は届き、`veil-load` の値は
48k / 46k（上）。`offload=True` は既定のまま、1 プロセス。

## 依存・リスク

- GIL の内部の振る舞いに依存しない（`take_gil` で待つことだけ）。3.13 以降の free-threaded は abi3 の wheel の対象外。
- 1 物理コアに縛ると Python と HTTP の処理が同じコアを分け合うので上限は下がる（ネイティブのハンドラの値が天井）。ハードウェアを明記して報告する。
