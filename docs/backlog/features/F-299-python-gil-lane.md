# F-299: Python ハンドラの経路（GIL のレーン・境界の削減）

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

## 計測

`python3 tools/perf/api_ab.py --scenario python`（`tools/api-bench/python/veil_app.py`。plaintext・json、2・4 スレッド、offload、async、
Python 3.12・3.14）。コア制限はサーバの CPU を固定して測る。`tools/api-bench/run_bench.py` にも目標の列を足す。

## 受け入れ条件

- 同じハードウェアで plaintext/json が 50k 以上（できれば 70k）。
- `offload=True` が既定のまま。ブロッキングなハンドラの並行性が落ちない（`time.sleep` のハンドラの試験）。
- FastAPI 風のデコレータの API を変えない。例外・検証エラー・`HTTPException` の挙動を変えない。
- `crates/veil-python` の試験と E2E が Python 3.9（abi3）〜3.14 で通る。

## 依存・リスク

- GIL の内部の振る舞いに依存しない（`take_gil` で待つことだけ）。3.13 以降の free-threaded は abi3 の wheel の対象外。
- 1 物理コアに縛ると Python と HTTP の処理が同じコアを分け合うので上限は下がる（ネイティブのハンドラの値が天井）。ハードウェアを明記して報告する。
