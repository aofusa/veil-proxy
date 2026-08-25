# F-139: `proxy_grpc_call` 系の真の双方向ストリーミング・接続プーリング

**ステータス: 完了（2026-08-25）**

## 完了時の実装（2026-08-25）

- **専用 gRPC 実行スレッド**（`src/server.rs::spawn_wasm_grpc_thread`、`src/entry.rs` で起動）。
  WASM tick スレッド（既定 100ms 周期）で状態機械を駆動すると 1 ステップ 100ms になり
  従来よりレイテンシが悪化するため分離した。アクティブな呼び出しが無い間は条件変数で待ち、
  あるときは `poll(2)` で「いずれかのソケットが読み書き可能 or 最短デッドライン」まで待つ
  （ビジースピンなし）。tick スレッド側の gRPC 実行ブロックは撤去した。
- **接続プール**（`src/wasm/host/grpc_pool.rs`）: `(host, port, tls)` ごとに HTTP/2 接続を再利用。
  1 接続 1 アクティブストリーム（チェックアウト / チェックイン）、アイドル 60 秒、
  GOAWAY / ストリーム ID 枯渇で破棄。呼び出しごとの TCP + TLS ハンドシェイクが消える。
- **ノンブロッキング状態機械**（`GrpcRunner` / `ActiveCall` / `GrpcEvent`）:
  `proxy_grpc_send` は half-close を待たずメッセージごとに即時送出、サーバーからのメッセージは
  到着ごとに `proxy_on_grpc_receive` へ配送する。送信ウィンドウ（接続・ストリーム両方）を追跡し、
  SETTINGS / WINDOW_UPDATE / PING / GOAWAY / RST_STREAM を処理する。デッドライン超過は
  `Close(DEADLINE_EXCEEDED)`。イベント順序は InitialMetadata → Message* → TrailingMetadata → Close。
- `execute_grpc_unary_call`（1 呼び出し 1 接続の同期実装）は単体テスト用の同期フォールバックとして温存。

**既知の制約**: 接続プールミス時の新規 TCP connect / TLS ハンドシェイクのみ同期実行する。
背景専用スレッド上でありデータプレーン（io_uring イベントループ）には影響しないが、
その間だけ他の呼び出しの進行が遅れる。理由は `src/wasm/host/grpc_executor.rs` の doc コメント参照。

---

## 背景

F-134 の調査で、`proxy_grpc_call`/`proxy_grpc_stream`/`proxy_grpc_send` が
pending call を登録するだけで実際に外向き gRPC 呼び出しを実行するループが
存在しなかった不適合を発見した。F-134 で以下を実装済み:

- `src/wasm/host/grpc_executor.rs`: gRPC-over-h2c/h2 の**ユーナリー**呼び出し
  （`proxy_grpc_call`）を、`src/server.rs` の WASM tick スレッド
  （io_uring イベントループとは別の専用バックグラウンドスレッド。
  ホットパス絶対規則に抵触しない）上でブロッキング I/O により実行する。
- `proxy_grpc_stream` + `proxy_grpc_send(end_of_stream=true)` の
  **クライアントストリーミング簡略版**: ゲストが送った全メッセージを
  `PendingGrpcUnaryCall::messages` に蓄積し、ストリームを half-close した
  タイミングでまとめて 1 回の HTTP/2 ストリームとして送出する。
- **TLS 上流**: `execute_grpc_unary_call` に `use_tls` を追加し、
  `src/wasm/http_executor.rs::execute_https_request` と同じ rustls +
  webpki-roots 検証で TLS（ALPN `h2`）上流に接続できるようにした。

本チケットは残る 2 点（真の双方向ストリーミング・接続プーリング）を扱う。

## 未対応 1: 真の逐次双方向ストリーミング

### あるべき姿

Proxy-Wasm の `proxy_grpc_stream`/`proxy_grpc_send`/`proxy_on_grpc_receive` は
本来、ゲストが `proxy_grpc_send` を呼ぶたび（`end_of_stream=false` でも）
即座にメッセージが送出され、サーバーからの応答メッセージも届き次第
非同期に `proxy_on_grpc_receive` へ配送される、真の双方向ストリーミングを
意図している。

### なぜ本チケットのスコープで実装しないか（技術的根拠）

1. **実行モデルの根本的な不一致**: 現在の gRPC 実行はすべて
   「WASM tick スレッドが 1 回のループ反復で 1 呼び出しをブロッキング I/O で
   完了まで実行する」設計（`src/server.rs::spawn_wasm_tick_thread` の
   `loop { cap_safe_sleep(tick_interval); ... }`）。真の双方向ストリーミングは
   「接続を tick 反復をまたいで開いたまま保持し、ゲストが `proxy_grpc_send` を
   呼ぶたびに追記送出し、非同期に届く受信フレームを都度
   `proxy_on_grpc_receive` へ配送する」という、**tick 反復単位ではなく
   接続単位の非同期状態機械**を要求する。これは現在の「pending call を
   1 回の tick で取り出して実行して結果を配送する」というシンプルなモデルを
   置き換える設計変更であり、H2C バックエンドプーリング（F-106）に匹敵する
   規模の新規サブシステムになる。
2. **ブロッキングモデルでの実装は tick スレッドを長時間占有する**:
   ブロッキング I/O のまま「接続を開いたまま次のゲスト送信を待つ」実装にすると、
   1 つの gRPC ストリームが tick スレッドを専有し続け、他の WASM モジュールの
   tick 処理・HTTP call 処理・他の gRPC 呼び出しがブロックされる
   （tick スレッドは単一）。ノンブロッキング化するには
   `mio`/`epoll` 相当のイベント多重化か、tick スレッドをストリーム数分
   増やすワーカープール化が必要で、いずれも本チケットの範囲を超える。
   **ホットパス絶対規則により io_uring イベントループ上でこれを行うことは
   禁止されている**ため（WASM 実行は協調的 yield を要求）、非同期化するなら
   独自の小規模イベントループを tick スレッド用に新設する必要がある。
3. **相関の複雑さ**: 現在の `PendingGrpcUnaryCall` は「呼び出し全体」を
   1 レコードで表すが、真のストリーミングでは「進行中の接続」を
   `call_id` → `(TcpStream, HTTP/2 stream state, 受信バッファ)` の
   マップとして tick 反復をまたいで保持する必要があり、`GLOBAL_PENDING_GRPC_CALLS`
   をレジストリからステートフルな接続テーブルへ作り直す必要がある。

### 改修案（着手する場合）

1. `GLOBAL_PENDING_GRPC_CALLS` を「新規呼び出しキュー」と
   「進行中接続テーブル（`call_id` → 接続状態）」に分離する。
2. tick スレッドの 1 反復で: (a) 新規呼び出しをノンブロッキング connect し
   接続テーブルへ登録、(b) 進行中の全接続に対して非ブロッキング
   `read`/`write`（`TcpStream::set_nonblocking(true)` + タイムアウト付き
   ポーリング、または `mio::Poll`）を 1 回ずつ試行し、受信フレームが
   揃ったら `proxy_on_grpc_receive` を配送、ゲスト側 pending send があれば
   送出する、というノンブロッキングイベントループ化が最小構成。
3. 接続クローズ（`proxy_grpc_close`/相手の END_STREAM）で接続テーブルから除去。

見積り: H2C バックエンドプーリング（F-106）と同程度以上の規模
（新規ステートマシン + ノンブロッキング I/O 多重化）。

## 未対応 2: 接続プーリング

### 現状

`execute_grpc_unary_call` は 1 呼び出し 1 TCP 接続（使い捨て）。同じ上流への
頻繁な WASM 発 gRPC 呼び出しがあると、毎回 TCP + (TLS の場合)ハンドシェイクの
往復が発生する。

### なぜ本チケットのスコープで実装しないか

- 未対応 1（真の双方向ストリーミング）と同様、**プーリングした接続を
  tick 反復をまたいで保持する**ことが前提になる。現在のユーナリー実装は
  「1 反復内で接続 open → 送受信 → close まで完結」という単純な形のおかげで
  `GLOBAL_PENDING_GRPC_CALLS` の設計が単純になっている。接続をプールに
  返却して次回再利用する形にすると、(a) 接続の生存確認（TCP レベルの
  half-close 検出）、(b) HTTP/2 の複数ストリーム多重化（1 接続で複数
  `call_id` を並行処理する場合は HPACK 動的テーブルの状態共有・
  ウィンドウ管理が必要）、(c) プールのエントリ数・アイドルタイムアウト等の
  設定が必要になり、単体の変更としては大きい。
- F-106（H2C バックエンドプーリング）が非同期 io_uring 経路向けに
  同種の問題を解いているが、実行モデル（非同期 vs tick スレッドの
  ブロッキング I/O）が異なるため直接流用はできない。
- 現状のユースケース（WASM モジュールから gRPC 上流への発呼）は
  高頻度なホットパスではなく（あくまで WASM フィルタからの補助的な
  外部呼び出し）、プーリング無しでも実用上の性能問題が確認されていない
  （計測は未実施だが、H1 の `proxy_http_call` も同様に非プール実装のまま
  運用されている）。

### 改修案（着手する場合）

未対応 1 のノンブロッキング接続テーブル化を行うなら、テーブルの
エントリを「呼び出し完了後も一定時間（アイドルタイムアウト）閉じずに
保持し、同じ `(host, port, use_tls)` への新規呼び出しが来たら再利用する」
形に拡張することで、双方向ストリーミング対応と同じ変更でプーリングも
実現できる。したがって **未対応 1 と 2 は同じ設計変更で同時に解決するのが
合理的**であり、分割実装は推奨しない。

## 優先度

P2（WASM から gRPC 上流への高頻度発呼が要件化した場合に着手）。
