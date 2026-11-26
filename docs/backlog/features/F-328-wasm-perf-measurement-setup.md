# F-328: Proxy-Wasm の経路の計測の準備（HttpArena と同じ要求の混ぜ方・張り直し・分類つきのプロファイル）

**状態: 完了（2026-11-26。[F-327](F-327-proxy-wasm-path-cost.md) の子）**

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
   手元の写しは `docs/artifacts/benchmark/work/HttpArena/requests/`）。回し方は gcannon に合わせる。gcannon のソース（`MDA2AV/gcannon` の
   `src/worker.c`。HttpArena の `docker/gcannon.Dockerfile` が取ってくる）で確かめた: 最初の接続でひな形を `(conn_offset + conn_idx) % n` で
   割り当て（接続の数で均等）、接続を張り直すたびに次のひな形へ移る（接続の中では替えない）。`-r N` は N 要求を送って N 応答を受け取ったら
   閉じて張り直し、そのソケットは `SO_LINGER` 0（RST で閉じる）。
   **済み（2026-11-26）**: `veil-load` の `--raw`・`-r`（`tools/perf/veil-load/src/h1.rs`。`--rate` と `-r`、h2 と `--raw` は一緒に使えない）。
   手元の試験用のサーバで、`-r 10` は 1 接続あたり約 10 要求・3 種が均等・エラー 0 を確かめた。
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

## 基準の値（2026-11-26、aofish、io_uring）

47a6e284（veil は main 3d8148a0 と同じ。veil-load の `--raw`・`-r` を足しただけ）を aofish でビルドし、3 つを交互に 3 巡（各 5 秒、1 秒の
暖機）。サーバは CPU 0-7・ワーカー 8 本、負荷は 8-15。値は中央値（括弧は 3 巡の最小〜最大）、CPU は 1 要求のサーバの user + sys（µs）。
Proxy-Wasm はエントリの設定（`entries/httparena/veil/veil.toml`）と当時のフィルタ（SDK、bulk-memory あり）、「bulk なし」は同じフィルタを
bulk-memory 無しでビルドしたもの、ネイティブは同じ口を Rust のハンドラで答える `veil-module`。

| 負荷 | Proxy-Wasm rps | CPU | bulk なし rps | CPU | ネイティブ rps | CPU |
|---|---|---|---|---|---|---|
| h1 GET（512 接続） | 459k（455〜487） | 16.64（15.66〜16.71） | 461k（454〜477） | 16.51（15.93〜16.79） | 721k（685〜729） | 10.50（10.39〜11.05） |
| h1 混ぜ方（GET・POST CL・POST chunked） | 372k（370〜402） | 20.54（19.09〜20.76） | 393k（386〜394） | 19.52（19.41〜19.86） | 654k（639〜659） | 11.64（11.61〜11.96） |
| h1 混ぜ方 + 10 要求ごとの張り直し | 240k（238〜255） | 31.37（29.26〜31.85） | 242k（241〜258） | 31.00（29.12〜31.18） | 414k（405〜420） | 17.69（17.40〜18.18） |
| h1 POST（Content-Length） | 362k（353〜382） | 21.14（19.95〜21.70） | 340k（327〜377） | 22.54（20.37〜23.35） | 671k（634〜672） | 11.31（11.29〜12.00） |
| h1 POST（chunked） | 349k（346〜372） | 21.90（20.48〜22.20） | 352k（348〜369） | 21.69（20.76〜21.88） | 639k（607〜645） | 11.93（11.76〜12.59） |
| json-tls（wrk、256 接続、`json-tls-rotate.lua`） | 197k（196〜218） | 38.97（35.17〜39.09） | 233k（227〜297） | 32.93（31.05〜33.62） | 423k（401〜526） | 17.40（16.72〜18.11） |
| h2（h2load `-c 64 -m 100`） | 1,079k（1,078〜1,182） | 7.25（6.61〜7.27） | 1,084k（1,075〜1,175） | 7.20（6.66〜7.25） | 2,260k（2,175〜2,375） | 3.45（3.29〜3.58） |
| h2（HttpArena の `-c 256 -m 100`） | 1,053k（1,052〜1,156） | 7.40（6.74〜7.40） | 1,070k（1,053〜1,152） | 7.31（6.78〜7.42） | 2,170k（2,124〜2,303） | 3.58（3.38〜3.66） |
| h3（h3load `-c 16 -m 64`） | 125k（125〜135） | 7.82（7.29〜7.88） | 126k（125〜133） | 7.74（7.36〜7.81） | 189k（188〜198） | 5.14（4.89〜5.14） |

- 揺れ: rps は 1 巡ごとに最大 10% 動く（どの版も 3 巡のうち 1 巡だけ高い。巡の順〈交互〉で決まる機械の状態と見られる）。比較は同じ表の
  中の交互の値で行い、CPU を主に見る（CPU の揺れは多くの行で 5% 以内）。
- json-tls の wrk は時々エラー（bulk なしで 487、ネイティブで 388。3 巡の合計）を出す。Proxy-Wasm とネイティブの両方に出るので負荷の側の
  事情（LibreSSL の wrk の再開）と見て、CPU で判断する。
- 張り直し（limited-conn の形）は 1 要求あたり +10.8 µs（Proxy-Wasm）・+6.1 µs（ネイティブ）で、Wasm の削減の効きは baseline の形より
  薄まる。F-327 の試算はこの比で直す。
- 未測定: epoll・h2c・Wasm 無し。後のチケットの A/B で epoll も測る（F-329 の A/B に epoll の基準がある）。

## 確かめたこと（2026-11-26）

- 回し方と張り直し（受け入れ条件の 1 つめ）: 手元の veil（JSON のアクセスログ）へ `veil-load --raw get.raw,post_cl.raw,post_chunked.raw
  -c 9 -t 1 -r 10 -d 2 --warmup 0`。ログの 26,519 要求はメソッドと本文の長さ（GET 0・POST 2〈Content-Length〉・POST 0〈chunked は
  ログに Content-Length の 0 を書く〉）で 3 分の 1 ずつ（暖機ありの 1 回目は 12,849・12,843・12,842）。サーバの側の接続の数（カーネルの
  `TcpPassiveOpens` の差）は 2,657 = 最初の 9 + veil-load の張り直し 2,648 で、1 接続あたり 9.98 要求（最後の接続は途中で終わる）。
- 分類つきのプロファイル（改修内容の 2）: aofish の手順は `docs/artifacts/proxy-wasm-perf/tools/prof12.sh`（計測の道具は git の外）。
  負荷を 2 秒流してから、5 秒の負荷の前後でサーバの全スレッドの `/proc/<pid>/task/*/stat`（utime・stime・minflt）を読んで 1 要求の
  user・sys を出し、続く 4 秒の負荷の間に `perf record -e cpu-clock:u -F 4000 -p <pid> -- sleep 3`（aofish の `perf` は `cpu-clock:u` のみ）。
  `perf report --no-children --sort dso,sym --stdio --percent-limit 0` の全シンボルを DSO（`.cwasm` = ゲスト）とシンボル名の型・
  モジュールのパスで分類し（ゲスト・ゲストからホストへの trampoline・wasmtime の出入り・ランタイム呼び出し・ホスト関数・veil の
  Proxy-Wasm の層・確保・`core::fmt`・`from_utf8`・libc の写し・タイマー・TLS・実行器・プロトコル）、割合 × user で µs/要求にする。
  ゲストとゲストから呼ぶ関数はファイバーのスタックの上で走り、子を含む合計は要求の処理の下に入らないので自己時間で分ける。
  結果の例は `docs/artifacts/proxy-wasm-perf/profile-2026-11-26.md`。
- 負荷の形（改修内容の 4）: json-tls は `json-tls-rotate.lua`、HTTP/2 は `-c 256 -m 100` も（上の表）。

## 受け入れ条件

- ひな形を回す負荷と張り直しが gcannon と同じ順と回数であることを、veil のアクセスログ（メソッド・本文の長さ）と接続の数で確かめる。
- 基準の値の表がある（同じ負荷を 2 巡し、揺れの幅も書く）。
- `tools/perf/veil-load` の README（または `tools/perf/README.md`）に新しい引数を書く。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-317](F-317-httparena-infra-score-plan.md)・F-295（`veil-load`）
