# F-324: HTTP/3 の 1 要求あたりの費用を減らす（veil-api 13.6 µs・nginx 6.3・agensio 1.8）

**状態: 完了（2026-11-26、feat/httparena-perf。下の「進捗」の 1〜3 を入れ、h3-plain の 1 要求の CPU は約 −10%・接続ごとのメモリは要求の数に比例しなくなった。受け入れ条件〈−30%〉には届かず、残りは quiche の中の作り直しが要る〈「次の候補」〉）**

## 今の状態（測った値）

HttpArena の baseline-h3（`h2load --alpn-list=h3 -c 64 -m 64 -t 64`）と static-h3（20 ファイル、`-m 64`）。2026-11-24 の結果:

| エントリ | baseline-h3 rps | 1 要求の CPU（µs） | 使ったコア | static-h3 rps | 1 要求の CPU（µs） |
|---|---|---|---|---|---|
| okena | 8.78M | 1.6 | 14 | 1.02M | 26.5 |
| agensio | 8.63M | 1.8 | 15 | 0.75M | 45.4 |
| nginx | 4.85M | 6.3 | 31 | 0.36M | 137.3 |
| veil-api | 2.64M | **13.6** | 36 | 0.29M | 142.2 |
| veil（Proxy-Wasm） | 0.53M | 75.6 | 40 | 0.01M | 1,296.3（B-495・B-496） |

- **使うコアが 36 で止まる理由**: QUIC の接続はカーネルの SO_REUSEPORT のハッシュ（4-tuple）でワーカーへ振り分けられる。64 接続を
  64 ワーカーへ無作為に配ると、接続の来ないワーカーが平均 23 本（64 × (63/64)^64）できて、2〜4 接続を持つワーカーが律速になる。
  nginx も同じ（31 コア）。上位 2 つは 1 要求が 2 µs 以下なので偏っても負荷ツールの上限（約 8.7M）に届く。**偏りを直すより、1 要求の
  費用を下げるのが本筋**（費用が 1/3 になれば、偏ったままでも約 8M に届く計算）。
- static-h3 は B-495・B-496 を直すと veil-api 並み（0.29M）になる見込み。その先は応答の本文（20 KB 前後、`-m 64`）の送出の費用。

## 1 要求でしていること（`src/http3_server/`）

- バッファ経路（静的配信・ハンドラ・Wasm・`Respond`）の要求は `H3BufferedTask`（約 250 バイト）を作ってワーカーの `FuturesUnordered`
  へ移し、作った直後に 1 回 poll する（B-97）。応答は `ProxyStream` のチャネルでメインループへ返し、メインループが quiche の
  `send_response`（QPACK の符号化）と `send_body_zc` を呼ぶ。
- quiche の中: ストリームの状態・フロー制御、STREAM フレームの組み立て、パケットの暗号化（AEAD・ヘッダの保護）、ACK の処理、
  GSO の束（F-305・F-309）、`mmsg_batch_size` の受信の束（F-151）。

## 改修内容（perf で上位に出た順に）

1. **計測**: aofish で HttpArena の h2load-h3 の Docker イメージ（`docker/h2load-h3.Dockerfile`）を作り、`-c 16 -m 64` 程度で
   `Respond`・ハンドラ・静的配信へ掛け、perf（aofish の perf コンテナ）でワーカーのサイクルの内訳を取る（veil・quiche・暗号・カーネル）。
   ノート PC には HTTP/3 の負荷ツールが無いので、交互 A/B は aofish で行う。
2. 候補（内訳を見て選ぶ）:
   - その場で終わる要求（`Respond`・キャッシュの当たり・すぐ返るハンドラ）は `FuturesUnordered` へ移さず、チャネルも通さずに
     メインループがそのまま応答を quiche へ渡す（F-323 の 1 と同じ考え方）。
   - 応答のヘッダは借用（`HeaderRef`）で渡す（`streaming.rs` の一部は `h3::Header::new` で名前と値を毎回 `Vec` に写している）。
   - QPACK: 応答のヘッダの静的表の索引が使われているかを確かめる（`:status 200`・`content-type` 等）。
   - 接続ごとの 1 周回の送出（`send` の束）に、同じ周回で終わった複数のストリームの応答が 1 パケットにまとまっているか（F-172 の
     STREAM の詰め合わせ）を、パケット数 / 応答数で確かめる。
   - 暗号: TLS 1.3 の AES-128-GCM になっているか（`[tls] cipher_suites` の順。F-314）。
3. 接続の振り分けの偏り（上の 36 コア）は、費用を下げた後に残る分を測ってから考える（新しい接続を空いたワーカーへ寄せるには、
   サーバの接続 ID に振り分け先を入れて CBPF で引く等の大きな設計が要る。ここでは案として記録するだけ）。

## 進捗

計測は aofish（F-317 の「計測の記録」と同じ条件。サーバは CPU 0-7・ワーカー 8 本、負荷は CPU 8-15 の h2load-h3）。`h3-plain` は
`Respond`（`/plain3`）、`h3-wasm` は Proxy-Wasm の `/baseline2`、`static-h3` は 20 ファイル（`Accept-Encoding` 付き）、いずれも
`-c 16 -m 64`。値は 1 要求あたりのサーバの CPU（µs）で、1 回 5 秒の交互 3 巡の中央値。

### 1. バッファ経路の要求をタスクのプールのスロットの中で作る

- 要求ごとに `H3BufferedTask` の future（数 KB）を作って `FuturesUnordered` へ移していた。移すたびに future 全体の写しと、
  `FuturesUnordered` のノードの `Arc` の確保・解放が 1 回ずつ要る。HTTP/2 のストリームと同じ `TaskPool::spawn_inline_with`
  （スロットの中で作ってその場で 1 回 poll する。F-158・F-300・F-323）にした。待ちに入った要求は実行器のタスクとして残り、
  応答を積むたびと送り終えたときに `ConnWaker` でメインループを起こす（メインループの select から `FuturesUnordered` の腕が消えた）。
- f319c → f324p: h3-plain 5.58 → 5.35（epoll、−4.1%）・6.07 → 5.52（io_uring、−9.1%）、h3-wasm 8.20 → 8.12（−1.0%）・
  9.27 → 8.76（−5.5%）、static-h3 31.03 → 31.32（+0.9%）・33.60 → 32.97（−1.9%）。h2c-plain（対照）は ±2%。

### 2. `classify` が引いたルートをタスクへ渡す・一時の `Vec` を使い回す

- メインループの `classify`（ストリーミングにするかの下見）とタスクの `h3_route_request` が、要求ごとに設定を読み
  （`CURRENT_CONFIG.load()`、1 回 100 命令超）、ルートを 2 回引いていた。`classify` が引いた値がワーカーのルートの表の値
  （`RouteMatch::Worker`）なら、`Wasm` のエンジンの写しと一緒にタスクへ渡す（`ClassifiedRoute`）。トラフィック制御のあるルートは
  下見では乱数を引かずに `Owned` を返すので（abort は常に当たり、分割は先頭）、下見が `Worker` を返すのは乱数の要らないルートだけで、
  本番の抽選と同じ値になる。`Owned` はタスクが引き直す。メトリクス・管理 API の要求は `classify` がルートを引く前にバッファ経路へ
  回すので、ルートを渡された要求はタスクでもルートの照合へ進む。`classify` の中のルートの照合も、入口で読んだ設定を使う
  （`resolve_route_in`）。
- 本文まで揃った要求・送り終えたストリームの ID を集める `Vec` を、束ごとに確保せずに使い回す（`ev_done`）。
- f324p → f324r: h3-plain 5.65 → 5.41（epoll、−4.2%）・5.70 → 5.88（io_uring、+3.2%）、h3-wasm 8.15 → 7.84（−3.8%）・
  8.94 → 8.96、static-h3 31.77 → 32.51（+2.3%）・32.63 → 32.87（+0.7%）。io_uring の h3-plain・static-h3 の増えは同じ組の中の揺れ
  （同じビルドの f324p-uring が前の比較では 5.52）の範囲。プロファイル（io_uring の h3-plain）で `memmove` 6.79 → 5.01%、
  `process_h3_events` 5.14 → 4.36%、`select_route`・ルートのキャッシュ（LRU）・`FuturesUnordered` が上位から消えた。
- 1・2 を合わせて h3-plain は epoll で約 −8%、io_uring で約 −6〜9%（3 を足して約 −10%）。

### 3. quiche: 片付けたストリームの ID を範囲で持つ（`third_party/quiche/README.veil.md` の 10）

- quiche の `StreamMap` は、完了して片付けたストリームの ID を `HashSet<u64>`（`collected`）に入れ、接続の終わりまで縮めない。
  HTTP/3 は要求 1 つでストリーム 1 本なので、集合は要求ごとに 1 項目ずつ増え、ストリームを作るたびの検索と集合の作り直し
  （倍々の再確保と写し）が続く。ストリームの種類ごとの通し番号の範囲（`RangeSet`）にした。範囲の切れ目は開いているストリームと
  相手が飛ばした番号だけで、どちらもストリームの数の上限に数えられるので、範囲の数は上限で抑えられる。
- メモリ（30 秒の h3-plain、`-c 16 -m 64`、交互 2 巡。負荷の後の RSS は同じ）: サーバのピークの RSS（VmHWM）が io_uring 131 → 56 MB、
  epoll 108 → 34 MB。約 580 万要求・16 接続で集合が約 75 MB になっていた。HttpArena の baseline-h3（64 接続・数百万 rps）では
  接続の数と要求の数に比例してもっと大きくなる。
- CPU（f324r → f324c、交互 4 巡）: h3-plain 5.06 → 4.96（epoll、−2.1%）・4.88 → 4.80（io_uring、−1.6%）、h3-wasm +0.8%・−1.7%、
  static-h3 +1.2%・+0.9%。プロファイル（io_uring の h3-plain）で `StreamMap::get_or_create` 3.9 → 2.4%、`__rust_alloc` 5.7 → 4.2%。
- **採らなかったもの**: quiche のストリーム ID のハッシュ（`StreamIdHasher`）は ID そのものを返す。hashbrown は上位 7 ビットで
  グループの中の候補を絞るので、小さな ID は全部同じ値になり、同じ種類の ID（4 の倍数）は表の 1/4 のバケットからしか始まらない。
  乗算で混ぜた版（f324h）は、範囲化（f324c）に比べて h3-plain が epoll +2.4%・io_uring +1.0%、h3-wasm +2.6%・+1.7% で効かなかった
  （プロファイルの `get_or_create` も 2.4 → 3.0%）。開いているストリームは 1 接続 64 本程度で、表が小さく衝突の費用が出ない。

### 残っている費用（f324r、io_uring の h3-plain のプロファイル）

- 上位は quiche: `StreamMap::get_or_create` 3.9%・`Connection::recv` 3.6%・`RecvBuf::ready` 3.3%・`process_readable_stream` 2.4%・
  `stream_do_send` 2.3%・`do_stream_recv` 2.2%・`SendBuf` 2.1%・優先度の赤黒木（`intrusive_collections`）合わせて約 5%。
  確保（`__rust_alloc` 5.7%・`VeilAlloc` 1.5%）と `memmove` 5.0% の多くも quiche のストリームの作成・破棄。
- veil 側: `process_h3_events` 4.4%・`handle_request_impl` 1.3%・`h3_check_field_section` 1.2%・arc_swap 1.6%（`classify` の
  設定の読み取り）。

### 次の候補（測った内訳から。どれも数 % ずつで、quiche か要求の処理の作り直しが要る）

- quiche の HTTP/3 のヘッダ: `h3::Header` は名前と値をそれぞれ `Vec<u8>` で持ち、要求・応答のヘッダ 1 項目ごとに確保が 2 回ある
  （QPACK の復号・`send_response` の両方）。ヘッダのブロック 1 つのバッファと範囲で持つ形にする。
- quiche のストリームごとの費用: `Stream` の作成と `HashMap` への移動（`memmove` 5〜6%）、優先度のキーの `Arc` と赤黒木
  （`intrusive_collections`、約 5%）、受信の `BTreeMap`（`RecvBuf::ready` 2〜3%）。
- その場で終わる要求（`Respond`・キャッシュの当たり）は、タスクと応答のチャネルを通さずにメインループが応答を quiche へ渡す
  （`handle_request_impl` のセキュリティの検査・圧縮・メトリクスを同期の関数へ切り出す作り直しが要る。HTTP/2 も同じ。F-323）。
- 接続の振り分けの偏り（「今の状態」の 36 コア）。

## 受け入れ条件

- aofish の h2load-h3（`Respond`、`-m 64`）で 1 要求の CPU が 30% 以上下がる。static-h3 も悪化しない。
- HTTP/3 の E2E（io_uring・epoll）・h3spec・`cargo test`。送出を変えたら h2load でも確かめる（AGENTS.md: quinn の E2E は素通りする）。

## 期待する効果

- baseline-h3 が 2.6M → 5M なら F-317 の試算で +約 270（S6）。veil-api の HTTP/3 の 2 プロファイルにも効く。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[F-323](F-323-h2-per-stream-fixed-cost.md)・B-97・F-151・F-172・F-305・F-309・F-311・F-314
