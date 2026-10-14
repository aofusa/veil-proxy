# F-211: VDS の機能強化（Envoy の xDS と比べた不足機能の洗い出しと計画）

**状態: 1〜3 完了（feat/api-roadmap-impl、2026-10-14）。4 以降は未着手（着手時に項目ごとのチケットに切る）**

## 今の VDS（F-04・F-199、`src/vds.rs`・`src/vds_grpc.rs`・`docs/guide/vds.md`）

- 配信するもの: `[[route]]` と `[upstreams.<name>]` のスナップショット全体（State-of-the-World）。リソースは TOML と同じ形の JSON。
- 転送: gRPC の双方向ストリーム（ADS と同じ形、`veil.vds.v1`）と REST の長輪詢。ACK・NACK（nonce・`error_detail`）。
- 適用: 設定ロードの単一の入口（`config_override::apply_to_toml_str`）でオーバーレイし、既存のリロード経路（検証 → ArcSwap）。
  失敗したら前の版を保つ。SIGHUP でもオーバーレイは残る。
- 接続: Bearer トークン（ファイル）・mTLS・追加 CA・TCP キープアライブ・指数バックオフ・応答サイズとリソース数の上限。
- 専用スレッド（`ctrl_http`）で動き、データプレーンに触れない。

## Envoy の xDS との比較

| xDS | Envoy の機能 | VDS | 不足 |
|---|---|---|---|
| RDS | ルート | ○ `Route` | ルート単位の更新ができない（全体の置き換え）。仮想ホスト単位（VHDS）・スコープ（SRDS）は無い |
| CDS | クラスタ | ○ `Upstream` | 同上 |
| EDS | エンドポイント（クラスタと別に配信、ロカリティ・優先度・エンドポイントごとの健康状態・重み） | △ `Upstream` の `servers` に含めて配信 | エンドポイントだけの変更でもスナップショット全体を送り直し、設定全体をリロードする。ロカリティ・優先度・制御プレーンからの健康状態（HEALTHY / DRAINING）は無い。重みはある |
| LDS | リスナー | × 再起動が要る | リスナーの追加・削除。サンドボックス（capsicum・pledge・Landlock・seccomp）の後に `bind` できない OS があるので、事前に確保したリスナー集合の中での有効・無効の切り替えに限る設計になる |
| SDS | 証明書・鍵 | × ファイルのホットリロードと ACME | 制御プレーンからの証明書配信（mTLS の制御プレーン接続が前提。鍵は zeroize・メモリのみ） |
| RTDS | ランタイムの値（機能フラグ・割合） | × | `[security]`・`[logging]`・レートリミット等、route・upstreams 以外の値。今は `VEIL_OVERRIDE_<n>` と SIGHUP |
| ECDS | 拡張（フィルタ）の設定 | × | Proxy-Wasm のプラグイン設定・モジュールの配信 |
| ADS | 1 本のストリームで全種類 | ○（スナップショット 1 種類） | – |
| Delta（増分）xDS | 変わったリソースだけ送る・削除の通知 | × | 大きな設定（`max_resources` 既定 10,000）で 1 件の変更ごとに全体を送り、全体を TOML 合成・解析・検証する |
| リソースの購読 | `resource_names` で欲しいものだけ・ワイルドカード・オンデマンド（ODCDS 等） | × 常に全部 | ノードごとの絞り込みは制御プレーン側の実装に任せている |
| リソースごとの版・NACK | リソースごとに版、NACK は該当リソース | × スナップショット単位 | 1 件の誤りで全体を拒否（今は意図した挙動。Delta と一緒に考える） |
| リソースの TTL・ハートビート | 制御プレーンが落ちたら期限で外す | × 最後の版を保ち続ける | 方針の選択肢として |
| ウォーミング・初回の待ち | クラスタは EDS が揃うまで使わない。`initial_fetch_timeout` | × 起動時はファイルの設定で動き始める | 「最初のスナップショットが届くまで受け付けない」設定（k8s のサイドカーで有用） |
| 制御プレーンの冗長化 | 複数の制御プレーン・フェイルオーバー・xDS フェデレーション（`xdstp://`） | × `server` は 1 個 | 複数の `server` を順に試す |
| ノード情報 | `node.id`・`cluster`・`metadata`・`locality`・ビルド情報・対応する拡張 | △ id・cluster・user_agent | metadata・locality（制御プレーンが出し分けに使う） |
| クライアントの状態（CSDS）・config_dump | 適用中の版・NACK の理由・設定の中身を管理 API で見る | × `vds::status()` はあるがどこからも参照されていない | **メトリクスと管理エンドポイントに出す**（接続中か・適用中の版・最後の NACK・最後の更新時刻・更新回数） |
| 負荷の報告（LRS）・健康診断の委譲（HDS） | 制御プレーンへ負荷・健康状態を返す | × | 将来 |
| Envoy の xDS とのバイナリ互換 | go-control-plane・Istio・Gloo 等がそのまま使える | × 対象外（`vds.proto` に明記） | 既存の制御プレーンを使えない。`envoy.service.discovery.v3` の ADS と RDS・CDS・EDS のサブセットを veil の設定に変換する入口を足すかを検討 |

## 優先度と計画

| 順 | 項目 | 理由 | 規模 |
|---|---|---|---|
| 1 | 状態をメトリクス・管理エンドポイントに出す（CSDS 相当） | 運用で「今どの版で動いているか・なぜ拒否したか」が見えない。`VdsStatus` は既にある | 小 |
| 2 | 制御プレーンの冗長化（`servers = [..]` を順に試す） | 制御プレーン 1 台の再起動・障害で配信が止まる | 小 |
| 3 | 最初のスナップショットまで待つ設定（`[vds] wait_initial = "10s"` 等。期限切れはファイルの設定で開始） | k8s で古い設定のまま要求を受けない | 小 |
| 4 | EDS 相当（エンドポイントだけの配信と、設定全体のリロードを通らない適用経路） | エンドポイントの入れ替え（オートスケール）が一番多い更新。全体リロードは重い | 中〜大（LB 状態・ヘルスチェッカーの保持。F-156 の「リスナーにつき 1 個」と整合させる） |
| 5 | Delta（増分）配信とリソース単位の版 | 4 と合わせて大きな設定の更新コストを下げる | 大 |
| 6 | node の metadata・locality、ロカリティ・優先度つきの LB | 複数ゾーンでの出し分け | 中 |
| 7 | SDS 相当（証明書の配信） | ACME を使わない内部 PKI | 中（鍵の扱いとサンドボックス） |
| 8 | RTDS・ECDS 相当（route・upstreams 以外の値、Proxy-Wasm の設定） | 再起動・SIGHUP 無しで変えたい値 | 中 |
| 9 | Envoy の xDS（v3）のサブセットを受ける入口 | 既存の制御プレーンの再利用 | 大 |
| – | LDS・LRS・HDS・TTL | サンドボックスとの両立・需要を見て判断 | 記録のみ |

1〜3 は本チケットの最初の作業として実装し（同じ `src/vds.rs` の範囲）、4 以降は着手時にチケットを切る。

## 受け入れ条件（1〜3）

- `veil_proxy_vds_connected`・`veil_proxy_vds_applied_info{version_info}`・`veil_proxy_vds_updates_total{result="applied|rejected"}`・
  `veil_proxy_vds_last_update_timestamp_seconds` を Prometheus に出し（名前空間は既存の `veil_proxy`）、管理エンドポイント
  （既存の `path_prefix`、既定 `/__admin`）の `GET /__admin/vds` で `VdsStatus` を JSON で返す。
- 複数の制御プレーンを指定でき、1 台目を止めると 2 台目から配信が続くことを `tests/standalone_e2e.rs` の VDS の試験で確かめる。
- 最初のスナップショットを待つ設定で、届くまでリスナーが受け付けない・期限で開始することを E2E で確かめる。
- `docs/guide/vds.md`（英日）・`examples/config.toml` を更新する。

## 関連

F-04・F-199（今の VDS）、`vds-xds-dynamic-config.md`（当初の構想）、F-196（k8s サイドカー）、F-156（ワーカーをまたぐ共有状態）。

## 実装（1〜3、2026-10-14）

1. **状態**: `VdsStatus` に `server`（今つないでいる制御プレーン）・`last_update_unix` を足し、`vds::status_json()` を
   管理 API の `GET {path_prefix}/vds`（HTTP/1.1・HTTP/2 の管理 API、`admin` feature。VDS 無効なら `{"enabled":false}`）で返す。
   Prometheus は `veil_proxy_vds_connected`・`veil_proxy_vds_applied_info{version_info}`・
   `veil_proxy_vds_updates_total{result}`・`veil_proxy_vds_last_update_timestamp_seconds`（VDS のスレッドが接続・適用・拒否の
   たびに更新し、スクレイプ時には何もしない。`[vds]` 有効時だけ登録）。
2. **冗長化**: `[vds] servers = [..]`（`server` より優先）。`ServerRotation` で、失敗したらすぐ次の制御プレーンへ、全部を 1 周
   したら従来のバックオフ。gRPC・REST の両方。制御プレーンを替えたら nonce は捨てる（版は SotW なのでそのまま送る）。
3. **初回の待ち**: `[vds] initial_fetch_timeout_ms`（既定 0 = 待たない。Envoy の `initial_fetch_timeout`）。`spawn_if_enabled` が
   VDS のスレッドを起動した後、最初の適用（Condvar）か期限まで起動を止める。リスナーはその後で開く。期限切れは警告して
   ファイルの設定で開始する。

試験: `vds::tests::server_list_and_rotation`、`tests/standalone_e2e.rs` の `f211_vds_failover_status_metrics_admin`
（gRPC・REST の両方で、1 台目を止めると 2 台目の内容が届く・`/__admin/vds` と `/metrics` の値・認証）と
`f211_vds_initial_fetch_wait`（制御プレーンが後から起動するまでリスナーが開かず、最初の応答から VDS の内容／期限で
ファイルの設定のまま開始）。文書: `docs/guide/vds.md`・`admin-api.md`・`observability.md`（英日）、`examples/config.toml`。
