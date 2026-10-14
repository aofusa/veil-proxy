# B-130: HTTP/2 で本文の受け手が閉じたストリームへの DATA が接続の受信ウィンドウを返さず、同じ接続が止まる

**状態: 完了（feat/api-roadmap-impl、2026-10-14。F-210 の Python の gRPC の相互運用試験で発見）**

## 症状

tonic のクライアントで上限（4MiB）を超える 5MiB のメッセージを送り RESOURCE_EXHAUSTED を受けた直後、同じ接続の
次の呼び出しが永久に止まる（HEADERS は送られるが DATA が出ない）。Python（ブロッキングの gRPC ハンドラ）で
数十回に 1 回。ネイティブの Rust のハンドラでは 25 回中 0 回（応答と残りの DATA の到着の順序の差）。

クライアントの h2 のトレースでは、接続の送信ウィンドウが 0 のまま WINDOW_UPDATE(0) が来ない。サーバ側は
gdb で全スレッドがアイドル（ワーカーは epoll_wait、ハンドラ用プールは Condvar 待ち）で、デッドロックではない。

## 原因

接続の受信ウィンドウ（65535）の補充が「ハンドラ・上流への中継が本文を受け取った」ときに限られ、次の 3 つの経路で
受信した DATA のクレジットを返していなかった（いずれもハンドラが本文を読み終える前に応答した場合）。

1. `proxy.rs` の多重化メインループ: 本文の受け手（`req_tx`）が既に閉じているストリームへの DATA を `req_pending` に
   積んだまま転送も補充もしなかった（ストリームの終了時の補充は、その時点で閾値を超えていなければ何もしない）。
2. `drive_h2_streams`: `try_send` が `Closed` のとき溜めていた本文を捨てたが、補充の対象にしていなかった。
3. `Http2Connection::handle_data`: 表に残っているが閉じた（こちらが RST_STREAM を送った・応答を終えた）ストリームへの
   DATA で `recv_data` がエラーになると、接続のウィンドウを消費したまま返していなかった（表から消えた後の DATA は
   B-121 で補充済み）。

## 修正

1. 受け手が閉じたストリームへの DATA は捨て、そのぶんをすぐ補充して送る。
2. `Closed` で捨てた分も補充の対象にする。
3. `handle_data` の `recv_data` のエラーでも、エラーを返す前に接続のクレジットを返す（閾値で WINDOW_UPDATE）。

## 試験

- 単体: `http2::connection::tests::data_on_closed_stream_still_in_table_replenishes_connection_window`
  （修正前は失敗する）。
- `tests/api/library_e2e.sh python`（python-grpc の plain・grpcio の両方で相互運用試験）。手元では修正後に
  plain で 1 スレッド・4 スレッド各 100 回、grpcio で 50 回、失敗 0。
