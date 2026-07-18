# F-124: L4 UDP プロキシ（セッションテーブル方式）

## 目的

L4 プロキシ（F-18）は TCP のみに対応していた。nginx stream の UDP / Envoy UDP
proxy 相当の UDP ストリームプロキシを追加し、DNS・syslog・SNMP・RADIUS など
UDP ベースのプロトコルを透過転送できるようにする。

## 現状（実装済み）

- `config.rs`: `L4ListenerConfig` に `protocol: L4Protocol`（`Tcp`（デフォルト）/
  `Udp`）を追加。TOML では `protocol = "tcp"|"udp"`。省略時は既存 TCP 構成と
  完全後方互換（`Tcp` のまま）。
- `runtime/udp.rs`: 汎用非同期 UDP ソケット `UdpSocket`（`bind`/`connect`/`send`/
  `recv`/`send_to`/`recv_from`/`local_addr`）。`recvfrom`/`sendto`/`connect`/
  `send`/`recv` を非ブロッキング fd に対して try-first で発行し、`EAGAIN` の
  場合のみ既存の `runtime::tcp::wait_readable_fd`/`wait_writable_fd`（io_uring
  では `IORING_OP_POLL_ADD`、reactor では poller 登録）で待機して再試行する。
  **新規 io_uring オペコードを追加しない**ため io_uring / reactor 両バックエンドで
  完全に共通の単一実装。HTTP/3(QUIC) 専用の `src/udp/socket.rs`
  （`QuicUdpSocket`、GSO/GRO 特化・`http3` feature ゲート）とは別実装。
- `l4/udp.rs`: UDP セッションテーブル方式の転送ロジック。
  - リスナー UDP ソケット 1 本（`Rc<UdpSocket>`）を全セッションで共有。
  - セッションテーブル `HashMap<SocketAddr, Rc<UdpSession>>`（クライアント
    アドレスをキー）。thread-per-core のため `Rc`/`RefCell` でロックレス。
  - 新規クライアントパケット到着時、既存のロードバランサ（`select_upstream`、
    ラウンドロビン/最小接続数）で upstream を選択し、`connect(2)` 済みの
    専用 UDP ソケットを割り当ててセッション化。
  - クライアント → upstream: リスナーの recvfrom ループが直接
    `session.upstream.send()`。
  - upstream → クライアント: セッションごとに 1 タスク（F-46 型付きタスクプール
    で spawn）が upstream ソケットを `recv` ループし、共有リスナーソケット
    経由で `send_to(client_addr)`。
  - アイドルタイムアウト: セッションは最終アクティビティ時刻を持ち、
    per-session タスクが短い間隔（`idle_timeout_secs` と 1 秒の小さい方）で
    `runtime::time::timeout` により upstream 受信を再試行しつつ、経過時間が
    `idle_timeout_secs` を超えたら自動退去する。
  - `max_connections`（0 = 無制限）は同時セッション数の上限として扱う。
  - パケットごとの新規ヒープ確保は行わない（受信バッファはリスナー/
    per-session タスクそれぞれで一度だけ確保しループ内で再利用。ホットパス
    絶対規則）。セッション作成自体はコネクションレスの性質上避けられない
    コールドパス処理として許容。
- `l4/server.rs`: `spawn_l4_listeners` が `config.protocol` で TCP（既存
  `handle_l4_connection` 経路）と UDP（新 `handle_l4_udp_listener` 経路）に分岐。
  UDP は accept ループの代わりに単一 UDP ソケットの recvfrom ループ。
- **TLS 非対応の扱い**: UDP は DTLS 非対象。`protocol = "udp"` かつ `tls` が
  `none` 以外の構成は、起動時に警告ログを出して `tls = none` へ強制する
  （起動を止めるハードエラーにはしない。設定ミス 1 件で他リスナーの起動まで
  巻き込むことを避けるため）。
- **ヘルスチェック**: UDP バックエンドのヘルスチェックも既存の TCP connect
  ベースのまま（`health.rs` は protocol を見ない）。UDP の到達性そのものの
  確認（プロトコル依存で一般化困難）は scope 外。
- ドキュメント: `examples/config.toml` に `protocol` フィールドの説明と DNS
  UDP プロキシの設定例を追記。README.md / docs/readme/README.ja.md の L4 節に
  UDP 対応を追記。
- テスト:
  - 単体: `config.rs`（`L4Protocol` の serde デフォルト/デシリアライズ）、
    `runtime/udp.rs`（bind/connect/send/recv/send_to/recv_from の往復）、
    `l4/udp.rs`（セッションテーブルの登録/検索、アイドルタイムアウト判定、
    `max_connections` 上限ロジック）。
  - E2E: UDP エコーバックエンドへ veil L4 UDP 経由で往復確認する E2E を追加
    （`tests/test_backends/udp_echo_server.py` + `tests/e2e_setup.sh` に起動を
    追加、`tests/e2e_tests.rs` に `test_l4_udp_echo_roundtrip`）。

## 受け入れ条件

- [x] `protocol = "udp"` の L4 リスナーが起動し、UDP エコーバックエンドへ
      往復できる。
- [x] `protocol` 省略時は既存 TCP 構成と完全後方互換。
- [x] UDP + `tls` 指定は警告して無視（none 扱い）。
- [x] セッションは `idle_timeout_secs` 無通信で自動退去する。
- [x] `max_connections` が同時セッション数の上限として機能する。
- [x] `cargo build`/`cargo clippy`（各 feature 組み合わせ）warning 0。
- [x] 単体・統合・E2E テストがすべて通過。

## 依存・リスク

- io_uring バックエンドに UDP 専用の新規オペコードを追加しないことを設計上の
  制約とした（ホットパス絶対規則: セキュリティサーフェス拡大の回避）。
  `wait_readable_fd`/`wait_writable_fd`（`IORING_OP_POLL_ADD`）を使った
  try-first ポーリングで代替しており、UDP は本質的にデータグラム単位のため
  TCP の splice(2) ゼロコピー転送（F-30）のような手法は適用できない
  （ユーザースペースでの `recv`/`send` コピーを許容）。
- UDP セッションはコネクションレスな性質上、正規のクライアントかどうかを
  区別する手段がない（送信元アドレス詐称に対する保護は L4 レイヤーでは
  scope 外。上位でのフィルタリング・レート制限は別途検討）。
