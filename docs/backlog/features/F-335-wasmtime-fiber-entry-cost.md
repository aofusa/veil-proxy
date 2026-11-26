# F-335: ファイバーの出入りの固定費を wasmtime の側で減らす（要判断）

**状態: 保留（[F-327](F-327-proxy-wasm-path-cost.md) の子。F-328〜F-334 の後に残る差を見て、利用者の判断を得てから着手する）**

## 現状

- [F-329](F-329-wasm-single-fiber-entry-per-request.md) の後も 1 要求 1 回の `call_async` が残る。1 回ごとに wasmtime 36 は、ファイバーの
  クロージャを `Box::new` し、スタックの先頭を初期化し、全ワーカー共有のエンジンの `Arc` を複製して捨て（共有の行への書き込み 2 回。
  コアが多いほど効く。HttpArena はサーバに 64 スレッド）、`StoreFiber` を破棄する。veil の連続呼び出し（`run_chain`）はホスト関数を
  挟むので、外側のトラップの準備（`setjmp`）と `Caller::with` が 1 段多い。プロファイル（h1 GET）で wasmtime の出入りは 2 回で 0.80 µs/要求。
- 多コアでの競合: aofish で 2 → 4 ワーカーにしたとき、Wasm に固有の上乗せはファイバーの出入りの約 +0.07 µs（`make_fiber` が 2.5 倍。
  `docs/artifacts/proxy-wasm-perf/profile-2026-11-26.md` の「ワーカーの数」）。HttpArena の 64 スレッドでどれだけ伸びるかは aofish では測れない。
- 常駐のファイバー（要求ごとにファイバーを作らない）: 非同期のホスト関数の中から直接ゲストを呼ぶ形は作れない（その中では wasmtime が
  ファイバーを止める文脈を取り出しているので、ゲストがエポックで譲るとパニックする。`src/wasm/exports.rs` の `allocate_guest_memory` の説明）。
  ans-02 の 3 が示した 2 層の形なら公開の API で作れる（`wasmtime-36.0.21/src/runtime/fiber.rs` の `BlockingContext::with` で、非同期の
  ホスト関数から戻ると文脈がストアへ戻ることを確かめた）: インスタンスごとに同期のホスト関数の「駆動のループ」を 1 回 `call_async` し、
  要求の間だけ非同期のホスト関数（`Func::wrap_async`）を `call_unchecked` で呼んで次の要求を待つ。

## 改修案（判断の材料）

| 案 | 内容 | 見込み（ans-02 の推測） | 費用と危険 |
|---|---|---|---|
| A. wasmtime の小さな手直し | Linux でも vendoring 版（`third_party/wasmtime`、BSD 向けに同じ 36.0.21 を持っている）を使い、ファイバーがエンジンの `Arc` を複製しない（ストアのエンジンを借りる）、クロージャを `Box` に置かない | h1 −0.25〜0.35 µs/要求 + 共有の書き込みが消える | 上流の版を上げるたびに手直しを持ち越す。[F-333](F-333-wasm-bulk-memory-libcalls.md) の `memory.copy` の手直しも同じ判断で入れられる |
| B. 公開 API の常駐ファイバー（2 層） | 上の形。インスタンスの `call_async` の future がストアを借り続けるので、要求の入出力は `HostState` の受け渡しの場所を通す。プールの枠が future を持ち、要求のタスクがそれを poll する | h1 −0.15〜0.25 µs/要求（待つたびの `Box<dyn Future>` と `call_unchecked` が残る）+ 共有の書き込みが消える | エンジンの大きな作り直し（ストアに直接触れている `settle`・`recycle`・`metering::arm` 等を全部駆動のループの中へ）。止めたまま捨てるときは 1 回の再開で必ず終える（2 回止まると `StoreFiber::drop` でパニック） |
| C. ワーカーごとの `Engine` | 共有のエンジンの `Arc` とプールの競合が消える | ファイバーの作成の費用は残る | プーリングアロケータの予約をワーカーで分ける、モジュールの読み込み（`.cwasm` の mmap は共有できる）・リロード・サンドボックスの適用の順（Landlock・seccomp の前に作る）を作り直す |
| D. wasmtime 38 以降へ上げる | ans-02 によると 38.0.0 で `setjmp`・`longjmp` が無くなり、トラップの準備が軽くなる（`wasmtime_setjmp` 0.47%・入れ子の `catch_traps` 0.87% の大部分） | h1 −0.10〜0.20 µs/要求 | BSD 向けの vendoring 版（B-55 等の手直し）も上げる必要がある。38 以降のセキュリティの修正の確認、`max_execution_time_ms`・取り消しの試験。版の番号・PR の番号は Gemini の報告（未確認） |
| E. 上流への提案 | A の手直しを上流へ（issue・PR は人が出す） | — | 時間がかかる |

[F-329](F-329-wasm-single-fiber-entry-per-request.md)（1 要求 1 回の出入り）の後に残る差と、上の見込みで利用者が判断する。

## 受け入れ条件

- 着手する場合に決める（1 回の出入りの費用と、64 コアに近い条件での共有の行の競合を測って効果を示す）。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)・B-55・`gemini/ans-01-host-path.md` の候補 2・6・`gemini/ans-02-wasmtime.md`
