# F-324: HTTP/3 の 1 要求あたりの費用を減らす（veil-api 13.6 µs・nginx 6.3・agensio 1.8）

**状態: 未着手（2026-11-25 起票。F-317 の子。測ってから効くものだけ行う）**

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

## 受け入れ条件

- aofish の h2load-h3（`Respond`、`-m 64`）で 1 要求の CPU が 30% 以上下がる。static-h3 も悪化しない。
- HTTP/3 の E2E（io_uring・epoll）・h3spec・`cargo test`。送出を変えたら h2load でも確かめる（AGENTS.md: quinn の E2E は素通りする）。

## 期待する効果

- baseline-h3 が 2.6M → 5M なら F-317 の試算で +約 270（S6）。veil-api の HTTP/3 の 2 プロファイルにも効く。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[F-323](F-323-h2-per-stream-fixed-cost.md)・B-97・F-151・F-172・F-305・F-309・F-311・F-314
