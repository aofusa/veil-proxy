# F-157: 平文 HTTP/2（h2c）の DATA フレームを `writev(2)` でゼロコピー送出する

- 優先度: P2
- 状態: 未対応
- 関連: F-74（HTTP/2 フレーム連結）、F-116（HTTP/2 多重化アクターモデル）、F-150（rustls 暗号文の writev 直結）、F-155/F-156（FreeBSD perf）
- 出典: `docs/artifacts/freebsd_h2c_l4_perf_analysis.md` の H10 / 改善案 4

## 背景

FreeBSD 実測（F-155 後、54,576B、3 反復中央値）で、h2c 平文の静的配信は
**対 nginx 0.61**（veil 40,381 / nginx 66,724）に留まっている。
TLS 版の HTTP/2（`h2_file_tls`）は対 nginx **1.16** で勝っているため、
「HTTP/2 のフレーミングそのもの」ではなく **平文経路固有の送出コスト**が疑わしい。

## 現状のコード

`src/http2/connection.rs` は F-74 でフレーム連結を導入済みで、
複数フレームを `write_buf: Vec<u8>` へ積んでから `flush_write_buf` で
**1 回の `write_all`** にまとめている（syscall 回数の観点では既に最適化済み）。

しかし `queue_data_frames`（2095 行付近）は

```rust
pub fn queue_data_frames(&mut self, stream_id: u32, data: &[u8], end_stream: bool)
```

というシグネチャで、**レスポンス本体を `write_buf` へ memcpy している**
（`frame_encoder.encode_data_into(&mut self.write_buf, ...)`）。
54KB の応答なら 1 レスポンスあたり 54KB の memcpy が 1 回発生する。

TLS 経路は F-150 で rustls の暗号文を `writev(2)` へ直接渡すようにしたが、
**平文経路にはこれに相当する最適化が無い**。

## 改修案

DATA フレームヘッダ（9 バイト）と本体 `Bytes` を別々の `iovec` として
`writev(2)`（`sendmsg`）へ渡し、本体の memcpy を消す。

## 見送った理由（F-156 時点）

**着手前に、以下を解決する設計が必要**:

1. **F-74 のフレーム連結との整合**。現在は「全フレームを 1 バッファに積んで 1 回書く」
   という不変条件で順序を保証している。DATA だけ別バッファ（iovec）にすると、
   HEADERS / WINDOW_UPDATE / SETTINGS 等の制御フレームとの**送出順序**を
   iovec の並びで正しく維持する仕組みが要る。`write_all` で「呼び出し境界では
   `write_buf` は空」という assert（449 行付近）が現在の順序保証の要になっている。
2. **フロー制御によるチャンク分割**。`queue_data_frames` は
   `max_frame_size` と接続/ストリームの送信ウィンドウで本体を分割しながら
   複数の DATA フレームを生成する。ゼロコピー化するとフレームごとに
   「9 バイトヘッダ + 本体スライス」の iovec ペアが必要になり、
   iovec 本数が増える（`writev` の `IOV_MAX` 制約も考慮が要る）。
3. **`&[u8]` から `Bytes` への変更**。呼び出し側が本体の所有権を
   `writev` 完了まで保持する必要があり、F-116 の多重化アクターモデル
   （ストリームごとの送信キュー）にまたがる変更になる。
4. **部分書き込みの再開**。`writev` は short write しうるため、
   「どの iovec の途中まで送ったか」を保持して再開する状態管理が要る
   （F-150 は rustls の `write_tls` が内部で完結させてくれるので不要だった）。

HTTP/2 の送信経路は F-116 の多重化アクターモデルで最も繊細な部分であり、
**期待値（分析ドキュメントは 40k → 65k rps を主張）は未実測**である。
また現在の FreeBSD 計測環境は B-66（平文大レスポンスの単調劣化）の影響で
交互 A/B の分解能が落ちており、この規模の変更の効果を確定できない。

したがって **B-66 の解消後に、上記 1〜4 の設計を固めてから着手する**こと。

## 着手前に確認すべきこと

- h2c 54KB の劣後が本当に memcpy 由来かを、まず**計測で切り分ける**
  （例: `truss`/DTrace で 1 リクエストあたりの `write`/`sendmsg` 回数とバイト数を数え、
  nginx と比較する）。memcpy ではなく syscall 回数やイベントループ周回数が
  主因なら、本チケットの改修は効かない。
- TLS 版 HTTP/2 が対 nginx 1.16 で勝っている事実との整合
  （同じフレーミングコードを通るので、平文固有の差分がどこにあるかを説明できること）。
