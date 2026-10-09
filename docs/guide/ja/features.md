# 機能一覧

[← ドキュメント目次](README.md) · [English](../features.md)

## 特徴

### コア機能
- **非同期I/O**: 独自実装 io_uring ランタイムによる効率的なI/O処理（データプレーンで monoio/tokio に非依存）
- **TLS**: rustls によるメモリ安全な Pure Rust TLS実装
- **kTLS**: rustls + 独自実装のカーネルTLSモジュールによるカーネルTLSオフロード対応（Linux 5.15+ / FreeBSD 13.0+ — F-126）
- **HTTP/2**: TLS ALPNネゴシエーションによるHTTP/2サポート（ストリーム多重化、HPACK圧縮・**4-bit LUT Huffman デコード** — F-121）
- **H2Cサーバー**: TLSなしのHTTP/2 Cleartext (H2C) サーバーサポート（Prior Knowledgeモード、RFC 7540 Section 3.4）
- **HTTP/3**: QUIC/UDPベースのHTTP/3サポート（quiche 低レベル sans-IO API: `Connection::recv`/`send`、輻輳制御・Pacing は `[http3]` で設定可能）。Linux io_uring バックエンドでは UDP データプレーンをパイプライン化: 受信は `mmsg_batch_size` 本の **`IORING_OP_RECVMSG`** を常時 in-flight、送信は **`IORING_OP_SENDMSG`**（GSO `UDP_SEGMENT` cmsg 付き）を複数 SQE で 1 回の `io_uring_enter` にまとめる（F-130。ホットパスに libc `recvmmsg`/`sendmmsg` なし。reactor ビルドや `VEIL_H3_MULTISHOT=0` 時は POLL + `recvmmsg`/`sendmmsg` にフォールバック）。真の `IORING_RECV_MULTISHOT` + 登録済み **provided buffer ring**（`IORING_REGISTER_PBUF_RING`）による受信も実装済み（F-130 C2）だが、**`VEIL_H3_BUFRING=1` のオプトイン**である: 開発・検証機のカーネル（Linux 6.8.0-137-generic）は `IORING_REGISTER_PBUF_RING` をクリーンなリングでも liburing 経由でも `-EINVAL` で拒否するため、multishot 受信経路を実機で一度も動かせておらず、既定では有効にしない。オプトインした場合も登録・アーム失敗時は自動的にパイプライン受信へフォールバックする。0-RTT接続確立
- **高速アロケータ**: mimalloc による高速メモリ割り当て + Huge Pages対応
- **高速ルーティング**: Radix Tree (matchit) によるO(log n)パスマッチング

### プロキシ機能
- **コネクションプール**: バックエンド接続の再利用によるレイテンシ削減（HTTP/1.1・HTTPS・**H2C/HTTP-2** バックエンド対応）。**上流 HTTP/2 は多重化する（F-174）**: 上流の HTTP/2 接続 1 本を 1 つのアクタータスクが所有し、複数の下流要求が同時に共有する。相手の `SETTINGS_MAX_CONCURRENT_STREAMS` を守り（満杯なら 2 本目の接続を張る）、GOAWAY 後は新しいストリームを割り当てない（`last_stream_id` より後のストリームは再送する）。要求本文は接続・ストリーム両方のフロー制御ウィンドウに従い WINDOW_UPDATE を待つ（B-106。旧クライアントは本文が接続ウィンドウを超えると「Send window exhausted」で失敗していた）。受信した DATA は下流が受け取ってから WINDOW_UPDATE するので、遅いクライアントはメモリを増やさず上流を止める。アクターは常にソケットを見ているため、上流がアイドル接続を閉じると即座にプールから外れる。拒否・GOAWAY されたストリームと、応答 head より前に失敗した冪等な要求は、新しいストリームで 1 回だけ再送する（F-177）。HTTP/1.1・HTTP/2・HTTP/3 のフロントエンドの `use_h2c` 上流で使う（HTTP/1.1 のフロントエンドは従来、要求ごとに接続を張っていた）
- **ロードバランシング**: 複数バックエンドへのリクエスト分散（Round Robin/Least Connections/IP Hash/Weighted/Consistent Hash）
- **ヘルスチェック**: HTTP/TCP/gRPCによるアクティブヘルスチェックと自動フェイルオーバー（HTTP: ステータスコード検証、TCP: 接続確認のみ、gRPC: Health Checking Protocol）
- **L4ストリームプロキシ**: TCP/UDPのロードバランシング（RoundRobin/LeastConn）、TLSパススルー（TCPのみ）、`splice(2)` によるカーネル内ゼロコピー転送（TCP、ユーザースペースバッファなし）、UDPはセッションテーブル方式＋アイドルタイムアウト退去、接続数/セッション数制限（`l4-proxy` feature が必要）
- **サーキットブレーカー**: サーバー単位のサーキットブレーカー（Closed→Open→HalfOpen）、Outlier Detection/排除、EWMAレイテンシ追跡（`metrics` feature が必要。リクエストリトライは未実装）
- **プロキシキャッシュ**: メモリ・ディスクベースのレスポンスキャッシュ（ETag/304、stale-while-revalidate、stale-if-error）
- **キャッシュPurge管理API**: HTTPでキャッシュ無効化（`PURGE`メソッド または `POST /__admin/cache/purge`）、exact/prefix/glob/all モードとBearerトークン認証
- **バッファリング制御**: 低速クライアントによるバックエンド占有防止のためのレスポンスバッファリング（Streaming/Full/Adaptiveモード）
- **WebSocketサポート**: Upgradeヘッダー検知による双方向プロキシ（Fixed/Adaptiveポーリングモード）
- **H2C (HTTP/2 over cleartext)**: TLSなしのHTTP/2バックエンド接続（gRPC対応）
- **H2Cサーバー**: HTTP/2 Cleartextサーバーサポート（Prior Knowledgeモード、内部ネットワーク向け）
- **ヘッダー操作**: リクエスト/レスポンスヘッダーの追加・削除（X-Real-IP, HSTS等）
- **リダイレクト**: 301/302/307/308 HTTPリダイレクト（パス保持オプション付き）
- **SNI設定**: HTTPSバックエンドへのIP直打ち時にSNI名を指定可能（仮想ホスト対応）

### HTTP処理
- **Keep-Alive**: HTTP/1.1 Keep-Alive完全サポート
- **Chunked転送**: RFC 7230準拠のChunkedデコーダ（ステートマシンベース）
- **Viaヘッダー**: RFC 7230 Section 5.7.1準拠のViaヘッダー挿入（プロキシチェーン追跡）
- **100 Continue**: RFC 7231 Section 5.1.1準拠のExpect: 100-continue対応
- **Hostヘッダー検証**: RFC 7230 Section 5.4準拠のHTTP/1.1必須Hostヘッダーチェック
- **Hop-by-hopヘッダー**: RFC 7230 Section 6.1準拠のヘッダー削除（Connection, Keep-Alive, TE等）
- **Rangeリクエスト**: RFC 7233準拠のRangeヘッダー解析と206 Partial Contentサポート
- **TEヘッダー**: RFC 7230 Section 4.3準拠のTEヘッダー解析（trailersサポート）
- **バッファプール**: スレッドローカルバッファプール（サイズ設定可能、メモリアロケーションオーバーヘッド削減）
- **レスポンス圧縮**: Gzip/Brotli/Zstdによる動的圧縮（Accept-Encodingネゴシエーション対応）

### パフォーマンス
- **CPUアフィニティ**: ワーカースレッドのCPUコアピン留め
- **CBPF振り分け**: SO_REUSEPORTのフローハッシュ（4タプル）ベースロードバランシング。同一接続を固定ワーカーへ振り分け（Linux 4.6+）
- **OpenFileCache**: ファイルメタデータキャッシュ（canonicalize、metadata、mime_guessのシステムコール削減） - 静的ファイル配信で60〜67%のシステムコール削減。キャッシュミス時はブロッキングな canonicalize/metadata/ディスク読込を専用オフロードスレッドプール（完了を eventfd + POLL_ADD で通知）で実行し、io_uring イベントループをブロックしない
- **静的ファイル本体キャッシュ**（F-146/F-150、既定オフ）: HTTP/2・HTTP/3 の静的配信は DATA フレーム/QUIC ストリームへの再フレーミングが必須で `sendfile(2)` を使えないため、従来はリクエストごとにファイル全体をオフロード経由で読み直していた。kTLS を無効化した HTTP/1.1 のユーザー空間 TLS（rustls）経路も同様に `sendfile(2)` を使えず、リクエストごとに `pread(2)` でファイルを読み直していた（F-150）。FreeBSD の推奨構成は `ktls_enabled = false`（software kTLS はレコード単位ディスパッチのため大きな応答で不利）であり、この経路に該当する。`[static_file_cache]`/`[route.static_file_cache]` で有効化すると、ファイル本体を `bytes::Bytes` としてユーザ空間メモリに保持し、キャッシュヒット時は `DashMap` ルックアップ + `Bytes::clone()`（参照カウント増加。HTTP/1.1 では Range リクエスト向けに `Bytes::slice()` も併用）のみで配信する（syscall・アロケーション・オフロード往復ゼロ）。kTLS 有効時・平文時の HTTP/1.1 は既に sendfile(2)/kTLS ゼロコピー経路を使うため対象外（無変更）
- **静的配信の圧縮結果キャッシュ**（F-169、既定オフ、`[static_file_cache]` に追従）: 上記の本体キャッシュがあっても、`[route.compression]` を有効にした HTTP/2・HTTP/3 の静的配信は従来リクエストごとに**同じ内容**を再圧縮しており、54KB の静的ファイル + zstd 圧縮構成では対 nginx で 0.3〜0.4× まで劣化させる支配的要因だった。`[static_file_cache]` が有効なとき、圧縮結果を `(パス, エンコーディング, 圧縮レベル)` 単位でメモリキャッシュする（レベルをキーに含めるため設定リロードで圧縮レベルが変わっても古い結果と混ざらない）。ヒット時は圧縮そのものをスキップし `Bytes::clone()` のみで返す。**専用の on/off キーは無い**: 本体キャッシュが有効なときにしか参照されないため、片方だけ有効にして素通りする（F-157 で踏んだのと同種の）事故が起こらない。メモリ上限も本体キャッシュの `max_entries`/`max_total_bytes` をそのまま使うため、圧縮結果ぶんが青天井に増えることはない。本体キャッシュのエントリが無効化・上書きされた場合（mtime 再検証・TTL 失効・読み込み失敗によるフォールバック）は、同じパスの圧縮結果も同じタイミングで必ず道連れに破棄される——古い内容から作った圧縮結果を返すことはない。対象は File ルートの静的配信のみで、プロキシ応答（毎回内容が異なり得る）はキャッシュしない。これとは独立に、zstd の圧縮コンテキスト（`zstd::bulk::Compressor`）自体もスレッドローカルに保持して使い回すようにした（静的・プロキシ双方の経路に効くが、確保が減ることとスループットが上がることは別の主張であり、これ単体でのスループット改善は約束しない）
- **HTTP/2 レスポンスストリーミング**: 非圧縮レスポンスは、バックエンドのボディを全バッファリングせず DATA フレームとして逐次転送する（`Content-Length` 既知・`Transfer-Encoding: chunked` の両方に対応）。chunked ボディは span ベースデコーダでゼロコピーにデコードする（読み取りバッファのサブスライスを使い中間 `Vec` を持たない）。各 DATA フレームは HTTP/2 フロー制御（コネクション/ストリームウィンドウ + `WINDOW_UPDATE`）に従うため、クライアントの受信速度に応じたバックプレッシャが効き、RSS がペイロードサイズに比例しない（大容量ダウンロードの OOM 耐性）
- **HTTP/2 送信フレーム連結（コアレッシング）**（F-74）: レスポンス送信経路では、1 レスポンス分の全フレーム（HEADERS + 全 DATA フレーム、gRPC トレイラー）を**接続ごとの再利用連結バッファ**（`write_buf`。スレッドローカルプールで接続をまたいで再利用）へ追記し、フレームごとに書き込む代わりに **1 回の `write_all` / io_uring 送信**でまとめて送出する。最小構成のレスポンスでも書き込みが 2 回以上から **1 回**になり、HEADERS が最初の DATA より先行してワイヤに出ることもない。フレーム追記は `encode_headers_into` / `encode_data_into` によりゼロ追加確保（per-frame `Vec` 無し）。大容量ストリーミングボディでは **128 KiB 閾値**（およびフロー制御 `WINDOW_UPDATE` 待ちに入る前）でフラッシュしてメモリを抑えつつ送信をパイプライン化し、`Content-Length` ストリーミング経路は HEADERS と最初の既読ボディ断片を連結する。制御フレーム（SETTINGS/PING/`RST_STREAM`/GOAWAY）は従来どおり直接書き込み（直接書き込みの呼び出し境界では連結バッファは常に空）、多重化ループの消費連動 `WINDOW_UPDATE`（F-116）は積まれたフレームを追い越さないよう `write_buf` へ追記するため、いずれの経路でもフレーム順序は保たれる
- **HTTP/2 リクエストストリーミング（アップロード）**: プロキシ対象のアップロードは、リクエスト **HEADERS** 受信時点で（ボディ完了を待たず）バックエンド接続を開始し、受信した各 `DATA` フレームを `Transfer-Encoding: chunked` のチャンクとして**ボディ全体をバッファせず**バックエンドへ転送する。フレームの所有バッファはゼロコピーで送出する（チャンクサイズ行と CRLF のみ小バッファ）。フロー制御のアカウンティングは `request_body` へコピーせず行う。バックプレッシャは**消費連動**（F-116）: 受信ウィンドウの補充（`WINDOW_UPDATE`）はボディ断片を per-stream リクエストタスクへ引き渡せたタイミングでのみ行うため、バックエンドが遅い場合は HTTP/2 フロー制御がクライアントを抑制し、未転送のアップロードバイトはウィンドウ分で有界に保たれる（RSS がアップロードサイズに比例しない）。適格条件は、HTTP/1.1（h2c 以外）の `Proxy` バックエンド・`buffering` モードが `full` 以外・WASM ボディフィルタ非適用・非 gRPC。ボディサイズ上限（`max_request_body_size`）は転送中に強制する（413 + `RST_STREAM`）。非適格なリクエストは従来のバッファ経路にフォールバックし挙動は変わらない
- **HTTP/2 ストリーム多重化（コネクション内並行処理、F-116）**: HTTP/2 サーバループは HTTP/3 と同型の**アクターモデル**を用いる。コネクションごとのメインループが `Http2Connection`（ソケット・HPACK 状態・フロー制御ウィンドウ・連結バッファ）を専有し**唯一のフレーム読み書き主体**となり、各リクエストは **per-stream タスク**（同一スレッドの io_uring executor 上に確保レス `TaskPool` で spawn）としてルーティング・セキュリティ・WASM・ファイル・バックエンド I/O を conn に一切触れずに実行する。タスクは有界の単一スレッド SPSC チャネル（`H2RespMsg`: Head/Body/Trailers/Reset。ボディは `bytes::Bytes` ゼロコピー）+ 起床 `Notify`（共有プリミティブは `src/stream_channel.rs`）でレスポンス断片を返し、メインループが HEADERS を**送信順に** HPACK エンコードし、DATA はコネクション/ストリーム送信ウィンドウの範囲だけ積んで（残りは `WINDOW_UPDATE` 到着まで保留）複数ストリームのフレームを 1 イテレーション 1 回の `write_buf` フラッシュへ合流させる。待機は **readiness ベース**（ソケットの `POLL_ADD` とタスク `Notify` を依存追加なしの 2-future select で race）であり、実行中の `read` をキャンセルしないため受信済みバイトが失われることはない。これによりコネクション内の Head-of-Line Blocking（従来は 1 リクエストのバックエンド往復完了まで次フレームを読めなかった）が解消され、`h2load -c100 -m10` では実効並列数がコネクション数 100 ではなくストリーム数 1000 になり、遅いバックエンド応答が同一コネクションの他ストリームを塞がない
- **HTTP/2 インライン初回 poll（readiness reactor バックエンド限定、F-158）**: **readiness reactor** バックエンド（kqueue = FreeBSD/OpenBSD/NetBSD/macOS、および Linux の `--features epoll`）では、per-stream タスクをエグゼキュータへ渡す前に**そのタスク自身の実 Waker で 1 回だけその場で poll** する。タスクが一度も pend せずに完了できる場合——`[static_file_cache]` と `[route.open_file_cache]` の両方がヒットする静的ファイル配信がまさにこれで、この経路は offload の往復が 0 回——そのまま完了し、**エグゼキュータへ一度も積まれない**。これにより「spawn → イベントループ 1 周 → 実行」という強制往復と、それに伴う確認用 `poll(2)`・`Notify` 起床が 1 リクエストあたり 1 回ぶん消える。pend した場合は通常 spawn と全く同じ状態で残る。FreeBSD 14.3 aarch64 での交互 A/B 10 ラウンド実測: **3B レスポンスで +27.3%**（中央値 555,119 → 706,779 req/s、10 ラウンド全勝・分布は完全分離）、**54KB で +0.8%**（退行なし）。消しているのは**リクエストあたりの固定費**なので、小さいレスポンスで支配的に効き大きいレスポンスでは埋もれる——この非対称性は設計どおりである。**Linux の io_uring バックエンドには意図的に適用しない**: io_uring では同じ待機が `IORING_OP_POLL_ADD` の SQE 1 本で他の SQE とまとめて submit されるため回収できる往復コストが小さく、実測で **-4.6% の退行**（12 ラウンド中 11 でベースライン勝ち）になった。そのため `src/runtime/uring/executor.rs` は 1 バイトも変更していない
- **ルート解決のゼロアロケーション化（F-159）**: `config::load_backend` は **1 リクエストごとに呼ばれる**（`upstream::find_backend_unified` がルート照合のたびに呼ぶ）にも関わらず、そこで毎回すべてを組み立て直していた: ルート単位の `security` / `compression` / `buffering` / `cache` のディープコピー（ローカル変数へ 1 回、`Arc::new` でもう 1 回の計 2 回）、単一 URL プロキシの `ProxyTarget::parse` + `UpstreamGroup` 生成、File ルートの `Arc<PathBuf>`/`Arc<str>`、圧縮・キャッシュ有効時の `info!` ログ、そして `mode = "memory"` に至っては**リクエストごとの `fs::read`**。ルート設定はリロードまで不変なので、`Backend` 本体・レスポンス圧縮設定・パスプレフィックスを**設定ロード時に 1 回だけ**解決して `Route` の `#[serde(skip)]` フィールドへ持たせる形にした（F-148 が WASM モジュールリストで導入したのと同じ形）。リクエスト経路は `Arc` の clone（参照カウント増分・malloc なし）だけになる。解決に失敗したルート（存在しないファイル等）は `None` のままにして従来の毎回構築経路へフォールバックするので、同じエラーがそのまま報告される。io_uring バックエンドでの交互 A/B 実測: **3B の h2c 静的配信で +7.41%**（95,707 → 102,796 req/s、6/6 ラウンド勝ち）、**54KB の h2c プロキシで +2.65%**（10,321 → 10,595 req/s、6/6）
- **HTTP/2 の per-stream 固定費削減（F-162）**: HTTP/2 は 1 ストリーム 1 リクエストなので `h2_spawn_for_request` の中の処理はすべてリクエスト単価になる。3 つの確保・パースを削った: `ActiveConnectionMetric::set_host` を `&str` 受けに（接続の最初のホストしか保持しないうえ、`metrics` 無効ビルドでも呼び出し側の `to_string()` だけは実行されていた）、クライアント IP を接続あたり 1 個の `Rc<str>` にして参照カウント clone で配る（従来はストリームごとに `Box<str>` を確保）、クライアント `SocketAddr` を 3 箇所で毎回パースしていたのを接続あたり 1 回に。**3B の h2c 静的配信で +0.89%**（固定費支配）。54KB プロキシでは 18 ラウンドで -0.29%（σ ≈ 1.3%）＝ノイズ内で、削ったものが「リクエストごとの小さな確保 2〜3 個」である以上これは想定どおり
- **WASM gRPC 呼び出しのノンブロッキング化と接続プーリング（F-139/F-160）**: `proxy_grpc_call` / `proxy_grpc_stream` / `proxy_grpc_send` は従来、WASM tick スレッド（100ms 周期）が 1 呼び出しずつブロッキングで完結させ、接続は 1 呼び出し 1 本の使い捨て、クライアントストリーミングは half-close までメッセージを溜め、応答も 1 メッセージにまとめて配送していた。現在は**条件変数 + `poll(2)` 駆動の専用 gRPC 実行スレッド**上で、**上流ごとの HTTP/2 接続プール**（`(host, port, tls)` キー・1 接続 1 アクティブストリーム・アイドル 60 秒・GOAWAY / ストリーム ID 枯渇で破棄）を使い、全 in-flight 呼び出しを 1 イテレーションずつ進めるノンブロッキング状態機械で実行する: `proxy_grpc_send` は都度送出し、応答メッセージは到着ごとに `proxy_on_grpc_receive` へ配送される。ゲストが渡したメタデータは直列化 `Bytes` のまま借用イテレータで走査して HPACK へ直接渡すため（F-160）、ホスト関数のホットパスでペアごとの `String` 確保が発生せず、メッセージ本体も `Bytes` 共有でディープコピーしない。プールミス時の新規接続だけは同期（背景スレッド上であり io_uring イベントループは一切関与しない）
- **HTTP/3 ストリーミング（双方向）**: quiche の `Connection`/`h3::Connection` は `Send` でなく単一スレッドの QUIC イベントループが駆動する必要があるため、HTTP/3 ストリーミングは**アクターモデル**（`src/http3_stream.rs`）を用いる。メインループが QUIC/H3 の I/O（`send_response`/`send_body`/`recv_body`）を専有駆動し、**バックエンドタスク**（同一スレッドの io_uring executor 上に spawn）がバックエンド TCP I/O を担う。両者は**単一スレッド SPSC 非同期チャネル**（`Rc<RefCell>`・**アトミック/ロックなし**）と起床 `Notify` で通信し、メインループは `select_biased!` でパケット受信・タスク起床・タイムアウトを多重化する。レスポンスボディ（`Content-Length`/chunked/EOF フレーミング。chunked は span デコーダでゼロコピーデコード）とリクエストボディ（`Transfer-Encoding: chunked` で転送）は `bytes::Bytes` でアクター境界をディープコピーせず受け渡す。有界チャネルにより**双方向バックプレッシャ**が効き（クライアント遅延→レスポンスチャネル満杯→バックエンド read 停止、バックエンド遅延→リクエストチャネル満杯→`recv_body` 停止→QUIC フロー制御でクライアント送信停止）、RSS はペイロードサイズに比例しない。リクエストの framing は**最初のボディ断片が実際に届いてから**確定する（h3 の GET は HEADERS と fin を別送するため `more_frames` だけではボディ有無を判定できない）。圧縮対象レスポンスは全バッファ後に圧縮する。**TLS バックエンドもストリーミングする**（F-44）: バックエンドタスクはハンドシェイク済み rustls セッションを全二重ラッパー（`TlsBackend`。内部可変性を用い、借用を `.await` を跨いで保持しない。kTLS 移行済みセッションは生ソケットの io_uring I/O）で包み、平文と同様にアップロードとレスポンス受信を同一タスク内で並行駆動する。適格条件は、HTTP/1.1（平文または TLS）または **h2c** の `Proxy` バックエンド・`buffering` が `full` 以外（gRPC は `full` をバイパス）・WASM モジュール非適用・セキュリティ許可。**h2c 上流は全二重で中継する（F-171）**: HTTP/3・HTTP/2 のどちらのフロントエンドでも、`use_h2c` 上流への要求は多重化した上流接続（F-174）のストリームとして開き、要求の DATA は届いた順に上流へ、上流の HEADERS / DATA / トレーラーは届いた順に下流へ流す。クライアントストリーミング・双方向ストリーミングの gRPC が成立する（最初の応答メッセージが要求の完了を待たない）。gRPC のトレーラーは HTTP/3 のトレーラー（`send_additional_headers`）/ HTTP/2 の末尾 HEADERS になり、trailers-only の gRPC 応答は fin / END_STREAM 付きの HEADERS 1 枚のまま返す。HTTP/1.1 上流への gRPC・WASM 適用ルート・`buffering = "full"`（gRPC 以外）はバッファ経路のまま。**バッファ経路もメインループを止めない（B-97）**: それ以外の要求（静的配信・メトリクス・gRPC・h2c 上流・`buffering = "full"`・WASM 適用ルート）は従来 QUIC のメインループが await しており、遅い上流・オフロードしたファイル読み込み・長い WASM 実行の間、そのワーカーの全 HTTP/3 接続が止まっていた。現在は要求ごとの Future をワーカー単位の `FuturesUnordered` に置き、メインループは await せずに poll する。応答（gRPC のトレーラーを含む）はストリーミング経路と同じ `ProxyStream` のチャネルで送出する。Future は作成直後に 1 回 poll するので、キャッシュに当たる静的配信は同じイテレーション内で送出される（3B の静的配信でインライン実行との差 2% 以内。別タスクとして spawn する方式では約 12% 退行した）。**上流のキープアライブ（B-104/B-105）**: HTTP/3 ワーカーから HTTP/1.1 上流への接続（平文・TLS、ストリーミング・バッファ経路とも）をワーカーごとにプールする（`[security] max_idle_connections_per_host` / `idle_connection_timeout_secs`。HTTP/1.1・HTTP/2 と同じ 1ms の `MSG_PEEK` 生存確認）。バッファ経路の HTTPS 上流で要求ごとにスレッドを生成していた同期 TLS 経路は廃止した

### 運用機能
- **Graceful Shutdown**: SIGINT/SIGTERMによる安全な終了
- **Graceful Reload**: SIGHUPによる設定のホットリロード（ゼロダウンタイム）
- **TLS証明書ホットリロード**: mtimeの変化検知＋ArcSwapによるゼロダウンタイム証明書ローテーション（既存接続は旧証明書を使い続け、新しいハンドシェイクのみ新証明書を自動適用）。**HTTP/1.1・HTTP/2 に加え HTTP/3（QUIC/quiche）も対応**し、秘密鍵の平文は全ワーカー適用後にゼロ化される
- **パニックリカバリー**: 接続レベルのパニックキャッチによるワーカースレッド復帰処理（影響は該当接続のみ）
- **非同期ログ**: ftlog による高性能非同期ログ
- **設定バリデーション**: 起動時の詳細な設定ファイル検証
- **Prometheusメトリクス**: メトリクスエンドポイントでリクエスト数、レイテンシ、アクティブ接続数、アップストリーム健康状態、サーキットブレーカー状態、コネクションプール統計、gRPC/WASM実行時間等を出力（要設定、デフォルト無効）。再起動なしでランタイム有効/無効切り替え対応
- **OpenTelemetry（OTLP/HTTP）**: Prometheusメトリクスを任意のOTLPコレクタ（Grafana Tempo, Jaeger等）へプッシュ配信。専用 std スレッドで完全tokio-free（`opentelemetry` feature が必要）

### セキュリティ
- **HTTP to HTTPSリダイレクト**: HTTPアクセスを自動的にHTTPSへ301リダイレクト
- **同時接続数制限**: グローバルな接続数上限設定
- **レートリミッター**: スライディングウィンドウ方式のレート制限
- **IP制限**: CIDR対応のIPアドレスフィルタリング
- **権限降格**: root起動後の非特権ユーザーへの降格
- **seccompフィルタ**: BPFベースのシステムコール制限 + mmap/mprotect の PROT_EXEC 引数レベル検証（オプション）
- **io_uringオペコード制限**: リング作成時に `IORING_REGISTER_RESTRICTIONS` を適用し、必要なオペコード（ACCEPT/RECV/SEND/SENDMSG/CONNECT/TIMEOUT/SPLICE/POLL_ADD）のみ許可
- **Landlockサンドボックス**: ファイルシステムアクセス制限（Linux 5.13+）
- **systemdサンドボックス**: 名前空間隔離・システムコール制限対応
