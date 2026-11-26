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

1. `veil-load` に、gcannon と同じ「要求のひな形を順に回す」（`--raw a.raw,b.raw,c.raw`）と N 要求ごとの張り直し（`-r N`）を足す。
   ひな形は HttpArena の `requests/get.raw`（60 B、`GET /baseline11?a=13&b=42`）・`post_cl.raw`（82 B、`Content-Length: 2`・本文 `20`）・
   `post_chunked.raw`（101 B、`2\r\n20\r\n0\r\n\r\n`）をそのまま使う（`scripts/lib/tools/gcannon.sh` の baseline・limited-conn の引数。
   手元の写しは `docs/artifacts/benchmark/work/HttpArena/requests/`）。回し方は gcannon に合わせる: gcannon の文書は「接続は張り直すときに
   ひな形を順に替える」（`-r` が無い baseline は接続ごとにひな形が決まったまま）としているが、ソースの行までは確かめていない（ans-04 の 7）。
   実装の前に gcannon のソース（`MDA2AV/gcannon`。HttpArena の `docker/gcannon.Dockerfile` が取ってくる）で、最初の割り当てと接続の中で替えるかを確かめる。
2. 分類つきのプロファイルの手順（`perf report --no-children --sort dso,sym` の全シンボルを、veil の Proxy-Wasm の層・wasmtime の出入り・
   ランタイム呼び出し・ホスト関数・ゲスト等に分けて µs/要求にする）を `tools/perf/` に置くか、計測の記録の手順に書く
   （aofish の `perf` は `cpu-clock:u` のみ）。
3. 共有の行の競合の兆候: ワーカーを 1・2・4・8 本にして、ワーカーあたりの負荷を揃えたときの 1 要求の費用が増えないかを見る
   （HttpArena はサーバに 64 スレッド。aofish は 8 本までしか見られない。wasmtime はファイバーごとに全ワーカー共有のエンジンの `Arc` を
   複製して捨てる〈[F-335](F-335-wasmtime-fiber-entry-cost.md) の調べ。F-335 は見送り〉）。2026-11-26 の予備の計測（`docs/artifacts/proxy-wasm-perf/profile-2026-11-26.md`
   の「ワーカーの数」）では、aofish はコアを増やすとクロックが下がって全分類が均等に伸び、Wasm に固有の上乗せは 4 ワーカーで約 0.07 µs
   （ファイバーの出入り）。SMT の兄弟と分け合うと Proxy-Wasm の版の方が大きく伸びる（×1.33 と ×1.09）。
4. 負荷の形を HttpArena に寄せる: json-tls は同じ `json-tls-rotate.lua`、HTTP/2 は HttpArena の `-c 256 -m 100` でも測る（aofish の既定は
   `-c 64 -m 100`）。limited-conn（10 要求ごとの張り直し）は張り直しの費用が大きく、Wasm の削減の効きが baseline より薄いので、F-327 の試算は
   この実測で直す。
5. 基準の値（main 3d8148a0）を、全負荷（h1 GET・POST〈CL・chunked〉・混ぜ方・張り直し・json-tls・h2・h2c・h3、Proxy-Wasm・ネイティブ・Wasm 無し）で
   測ってこのチケットに残す。json-tls は既定のフィルタと bulk-memory 無しのフィルタの両方で測る（`memory.copy` の 2.9 µs が他の変更の効きを
   薄めるので、後のチケットの A/B は bulk-memory 無しのフィルタでも見る）。

- 2026-11-26 の調査で測った値（main 3d8148a0、io_uring）は [F-327](F-327-proxy-wasm-path-cost.md) の表（h1 GET・h1 POST〈Content-Length〉・
  json-tls・h2・h3）。chunked・混ぜ方・張り直し・epoll は未測定。

## 受け入れ条件

- ひな形を回す負荷と張り直しが gcannon と同じ順と回数であることを、veil のアクセスログ（メソッド・本文の長さ）と接続の数で確かめる。
- 基準の値の表がある（同じ負荷を 2 巡し、揺れの幅も書く）。
- `tools/perf/veil-load` の README（または `tools/perf/README.md`）に新しい引数を書く。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-317](F-317-httparena-infra-score-plan.md)・F-295（`veil-load`）
