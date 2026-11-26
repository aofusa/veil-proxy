# F-335: ファイバーの出入りの固定費を wasmtime の側で減らす（要判断）

**状態: 保留（[F-327](F-327-proxy-wasm-path-cost.md) の子。F-328〜F-334 の後に残る差を見て、利用者の判断を得てから着手する）**

## 現状

- [F-329](F-329-wasm-single-fiber-entry-per-request.md) の後も 1 要求 1 回の `call_async` が残る。1 回ごとに wasmtime 36 は、ファイバーの
  クロージャを `Box::new` し、スタックの先頭を初期化し、全ワーカー共有のエンジンの `Arc` を複製して捨て（共有の行への書き込み 2 回。
  コアが多いほど効く。HttpArena はサーバに 64 スレッド）、`StoreFiber` を破棄する。veil の連続呼び出し（`run_chain`）はホスト関数を
  挟むので、外側のトラップの準備（`setjmp`）と `Caller::with` が 1 段多い。プロファイル（h1 GET）で wasmtime の出入りは 2 回で 0.80 µs/要求。
- インスタンスごとに止めたまま持つファイバー（要求ごとにファイバーを作らない）は、公開の API では作れない: 非同期のホスト関数の中では
  wasmtime がファイバーを止める文脈を取り出しているので、その中から呼ぶゲストはエポックで譲れない（`src/wasm/exports.rs` の
  `allocate_guest_memory` の説明）。

## 改修案（判断の材料。ans-02 の結果を足す）

1. wasmtime を Linux でも vendoring 版（`third_party/wasmtime`、BSD 向けに同じ 36.0.21 を持っている）にして小さく手直しする:
   ファイバーがストアのエンジンを借りる（`Arc` を複製しない）、クロージャをファイバーのスタックに置く（`Box` を作らない）、連続呼び出しを
   ホスト関数を挟まずに `on_fiber` で走らせる、インスタンスごとに止めたまま持つファイバー。上流の版を上げるたびに手直しを持ち越す費用がかかる。
2. ワーカーごとの `Engine`: 共有のエンジンの `Arc` とプールの競合が消える。手直しは要らないが、プーリングアロケータの予約（`total_memories` を
   ワーカーで分ける）・モジュールの読み込み（`.cwasm` の mmap は共有できる）・リロード・サンドボックスの適用の順（Landlock・seccomp の前に作る）
   を作り直す。
3. 上流への提案（issue・PR は人が出す）。

## 受け入れ条件

- 着手する場合に決める（1 回の出入りの費用と、64 コアに近い条件での共有の行の競合を測って効果を示す）。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)・B-55・`gemini/ans-01-host-path.md` の候補 2・6・`gemini/ans-02-wasmtime.md`
