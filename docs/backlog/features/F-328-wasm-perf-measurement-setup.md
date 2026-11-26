# F-328: Proxy-Wasm の経路の計測の準備（HttpArena と同じ要求の混ぜ方・張り直し・分類つきのプロファイル）

**状態: 未着手（[F-327](F-327-proxy-wasm-path-cost.md) の子。最初に行う）**

## 現状

- aofish の計測は `tools/perf/veil-load` の h1 GET（`/baseline11?a=1&b=1`）・h2c、h2load（h2）、wrk（json-tls）、h3load（h3）。
- HttpArena の baseline・limited-conn は gcannon の 4096 接続で GET・POST（Content-Length）・POST（chunked）の 3 種を回し、limited-conn は
  10 要求ごとに張り直す（F-317）。要求の 3 分の 2 が POST だが、POST の負荷を測っていない。
- `veil-load` は要求の方法と本文が全体で 1 つ（`--method`・`--body-file`。Content-Length のみ）で、chunked と張り直しが無い。
- 効果の帰属: Proxy-Wasm の経路はゲストとゲストから呼ぶ関数が wasmtime のファイバーのスタックの上で走り、`perf` の子を含む合計が
  要求の処理の下に入らない。2026-11-26 の調査では全シンボルの自己時間を分類して出した（`docs/artifacts/proxy-wasm-perf/profile-2026-11-26.md`）。

## 改修内容

1. `veil-load` に、HttpArena と同じ要求の混ぜ方を足す: 接続ごとに GET・POST（Content-Length）・POST（chunked）を順に回す
   （`--mix`。本文と chunked の区切りは gcannon に合わせる）、N 要求ごとに張り直す（`--reqs-per-conn N`）。
2. 分類つきのプロファイルの手順（`perf report --no-children --sort dso,sym` の全シンボルを、veil の Proxy-Wasm の層・wasmtime の出入り・
   ランタイム呼び出し・ホスト関数・ゲスト等に分けて µs/要求にする）を `tools/perf/` に置くか、計測の記録の手順に書く
   （aofish の `perf` は `cpu-clock:u` のみ）。
3. 共有の行の競合の兆候: ワーカーを 1・2・4・8 本にして、ワーカーあたりの負荷を揃えたときの 1 要求の費用が増えないかを見る
   （HttpArena はサーバに 64 スレッド。aofish は 8 本までしか見られない。wasmtime はファイバーごとに全ワーカー共有のエンジンの `Arc` を
   複製して捨てる〈[F-335](F-335-wasmtime-fiber-entry-cost.md)〉）。
4. 基準の値（main 3d8148a0）を、全負荷（h1 GET・POST〈CL・chunked〉・混ぜ方・張り直し・json-tls・h2・h2c・h3、Proxy-Wasm・ネイティブ・Wasm 無し）で
   測ってこのチケットに残す。

- 2026-11-26 の調査で測った値（main 3d8148a0、io_uring）は [F-327](F-327-proxy-wasm-path-cost.md) の表（h1 GET・h1 POST〈Content-Length〉・
  json-tls・h2・h3）。chunked・混ぜ方・張り直し・epoll は未測定。

## 受け入れ条件

- 混ぜ方と張り直しの負荷が HttpArena の gcannon の要求と同じ形（要求行・ヘッダ・本文・chunked の区切り）であることを、veil のアクセスログか
  パケットの写しで確かめる。
- 基準の値の表がある（同じ負荷を 2 巡し、揺れの幅も書く）。
- `tools/perf/veil-load` の README（または `tools/perf/README.md`）に新しい引数を書く。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-317](F-317-httparena-infra-score-plan.md)・F-295（`veil-load`）
