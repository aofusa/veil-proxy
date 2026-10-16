# F-228: Istio の ambient の ztunnel を veil で置き換える

**状態: 未着手（2026-10-16 起票・設計。親 F-215 の残件。F-224 で「検討のみ」としたもの）**

## 目的

ambient のノードごとの L4 のプロキシ ztunnel（Rust 製、DaemonSet）を veil にする。これで、ambient のデータプレーン
（ztunnel と waypoint）がすべて veil になる。waypoint は F-224 で実装済みである。制御プレーン（istiod・istio-cni）は改造しない。
本家の ztunnel が動くノードとの混在も通す。

## ztunnel の仕事（段階 0 で 1.31.1 のソースと実機の記録で確かめる）

以下は設計の前提である。段階 0 で、Istio 1.31.1 のタグの `ztunnel`・`istio/cni` のソースと実際のやり取りの記録を突き合わせて
確かめ、違っていればこの節を直してから実装に入る。

- **Pod の登録（ZDS）**: istio-cni のノードのエージェントは、ambient に入った Pod の network namespace の fd を UDS
  （`/var/run/ztunnel/ztunnel.sock`）で ztunnel に渡す。`SCM_RIGHTS` を使い、protobuf の `zds.proto` の
  `WorkloadRequest`（add・keep・del・snapshot_sent）に載せる。ztunnel は `WorkloadResponse`（ack）を返す。
  ztunnel が再起動したときは、istio-cni が全 Pod を送り直し（snapshot）、最後に snapshot_sent を送る。
- **Pod の中のリスナー（in-pod モード）**: ztunnel は各 Pod の network namespace の中に次のリスナーを作る。
  - 15001: outbound。Pod の iptables が捕まえる。
  - 15006: 平文の inbound。
  - 15008: HBONE の inbound。
  - 15053: DNS のプロキシ。任意。
  上流へも Pod の名前空間の中から接続する。送信元 IP が Pod の IP のままになり、自分の iptables を避ける印（`SO_MARK`）を付ける。
- **設定（xDS）**: istiod の 15012 から Delta で `istio.workload.Address`（Workload と Service）と
  `istio.security.Authorization` を受ける。ノードのサービスアカウントのトークンで認証する。
- **証明書**: 各ワークロードの ID の証明書を、Istio の CA（`IstioCertificateService/CreateCertificate`）にノードの代理として
  要求する（ID のなりすましのメタデータを付ける）。要求は必要になってからで、有効期間の半分で更新する。
- **転送**:
  - outbound: 宛先（VIP か Pod の IP）を Address の表で引く。
    - waypoint があれば、waypoint へ HBONE で送る。
    - 無ければ、宛先の Pod の IP:15008 へ HBONE で送る（mTLS で宛先の ID を確かめる）。
    - メッシュ外の宛先は、そのまま TCP で送る。
  - inbound の HBONE: 宛先のワークロードの証明書で mTLS を終端する。Authorization（L4）を評価し、Pod の中のポートへ接続する。
- **管理**: 15000（`/config_dump`。`istioctl ztunnel-config` が読む）、15020（メトリクス。`istio_tcp_*` を reporter が
  source / destination で）、15021（`/healthz/ready`）。

## 設計

### 起動と配布

- argv[0] が `ztunnel` のときに ztunnel として動く。feature は `mesh-ztunnel` とし、`mesh-istio` に含める。default・`full*` には入れない。
- イメージ: `docker/Dockerfile.mesh-ztunnel`（`FROM istio/ztunnel:1.31.1`、musl 版の veil を `/usr/local/bin/ztunnel` に置く）。
- 入れ替え: `istioctl install --set values.ztunnel.image=...`、または helm の `ztunnel` チャートの `image`。
- 引数（`proxy ztunnel`）と環境変数（`CA_ADDRESS`・`XDS_ADDRESS`・`ISTIO_META_*` 等）は、段階 0 で記録したものに合わせる。

### Pod の名前空間とワーカー

- `setns(2)` はワーカーのスレッドでは呼ばない（ホットパスで呼ばない）。名前空間をまたぐ仕事は、専用の「名前空間スレッド」
  1 本がまとめて行う。手順は `setns`（Pod）→ `socket`・`setsockopt`・`bind`・`listen` → `setns`（ノード）。
  作ったソケットは、作られた名前空間に属したまま使える。
- リスナー: Pod ごと・ポートごとに、ワーカーの数だけ `SO_REUSEPORT` のリスナーを名前空間スレッドが作り、各ワーカーに渡す。
  リスナーにつき 1 個にする状態（接続数など）は Pod 単位で持つ（F-156）。
  - Pod の追加と削除は、各ワーカーのリスナーの集合の差し替えで行う。ワーカーのイベントループに、名前空間スレッドからの通知を
    1 本足す（`offload` の通知 fd と同じ仕組み）。
  - 規模の目安: 1 ノード 110 Pod × 3 ポート × 4 ワーカーで約 1,300 fd。`RLIMIT_NOFILE` を引き上げる（B-44）。
- 上流へのソケット: 接続のたびに `setns` しないよう、ワーカーごと・Pod ごとに、未接続のソケットを数本ためておく。
  - ソケットは名前空間スレッドが作る（`SO_MARK` は作るときに付ける）。
  - 接続のときは取り出すだけで、syscall は増えない。
  - 減ったらワーカーのアイドル点で補充を頼む。空のときは名前空間スレッドへの依頼を `offload` と同じ形で await する
    （スレッドは止めない）。
  - ためる本数は Pod ごとの接続のレートから決め、上限を設ける。
- io_uring・epoll のどちらでも、作ったソケットは通常の fd として渡す。新しい io_uring のオペコードは足さない。

### 制御プレーン・証明書・転送

- **xDS**: `src/mesh/xds/ads.rs` の Delta のクライアントを使う。
  - `Address` の Workload の解読は `src/mesh/peer.rs`（WDS）にあるので、Service を足して、宛先の表（VIP・Pod の IP →
    ワークロード・waypoint・ポートの対応）にする。
  - `Authorization` は `src/mesh/xds/rbac.rs` の L4 の部分に写す（接続時に 1 回だけ評価する。F-220 と同じ）。
- **証明書**: CA の gRPC のクライアントを足す。
  - 鍵と CSR は rcgen で作る（Linkerd の CSR は `linkerd2-proxy-identity` が作るが、ztunnel では自分で作る）。
  - ID ごとに `Arc` の証明書を持ち、差し替えは次のハンドシェイクから（F-217 と同じ）。
- **HBONE**: `src/mesh/hbone.rs` の CONNECT の送受信と、宛先ごとの HTTP/2 の多重化接続を使う。
  - inbound は、宛先の IP からワークロードの ID を決めて、その証明書で mTLS を終端する。
  - 宛先の ID の確認は SPIFFE の SAN（F-217）。
- **平文の inbound（15006）とメッシュ外への outbound**: L4 の転送（`src/mesh/tcp.rs`）。
- **DNS のプロキシ（15053）**: 段階 4 まで後回し。Istio の DNS の捕捉が有効なメッシュでは、最初は受け付けない旨を起動時に出す。
- **管理・メトリクス**:
  - `/config_dump` は `istioctl ztunnel-config workloads|services|policies|certificates` が読む JSON。段階 0 の記録をフィクスチャにする。
  - `istio_tcp_*` のラベルは、接続時に Workload から解決する（F-221）。

### 権限

ztunnel の DaemonSet は、`setns` に `CAP_SYS_ADMIN`、`SO_MARK` に `CAP_NET_ADMIN`（と `NET_RAW`）を付けて動く。veil 側で
追加の権限は要らない。メッシュのモードでは veil 自身の seccomp・Landlock を使っていない。将来使うなら、`setns` を許可リストに
入れる（B-103 の手順）。

## 段階

| 段階 | 内容 |
|---|---|
| 0 | 前提の確認と記録。本家の ztunnel を kind で動かし、ZDS（UDS のやり取りを strace / 記録用の中継）・xDS（`VEIL_MESH_XDS_RECORD` 相当）・CA の要求・`/config_dump` を `tests/fixtures/istio/1.31.1/ztunnel/` に保存し、変換の試験の入力にする |
| 1 | ZDS の受け・名前空間スレッド・Pod の中のリスナー・L4 の平文の転送（メッシュ外の宛先・15006）。ノード 1 台で、ambient の Pod の間の TCP が流れる |
| 2 | 証明書（CA）・HBONE の送受信・mTLS の ID の確認。ambient の bookinfo が veil の ztunnel だけで動く |
| 3 | Authorization（L4）・メトリクス・管理 API・istioctl ztunnel-config・waypoint（veil / 本家）との組み合わせ |
| 4 | 混在（kind のワーカー 2 台で、片方を本家の ztunnel）・カオス（ztunnel の再起動と istio-cni の snapshot、istiod の停止）・DNS のプロキシ・性能 |

## 受け入れ条件

- （必須、F-215）既存のプロキシの性能が退行しない（`suite_ab.sh`）。ztunnel のコードは full のバイナリに入らない。イメージは musl 版。
- `tools/k8s/mesh-e2e.sh` に ztunnel の組（`ambient-ztunnel`）を足し、stock / veil / mixed（ノード単位の混在）で次を通す。
  - ambient の Getting Started（bookinfo）。
  - L4 の AuthorizationPolicy の許可・拒否。
  - waypoint を挟んだ L7 の認可とトラフィックの移行。
  - Kiali の辺（mTLS の鍵マーク）。
  - ztunnel の再起動からの復帰。
- `istioctl ztunnel-config` の各表示に、veil の ztunnel のデータが出る。
- 性能: 本家の ztunnel と、`mesh-bench.sh` の ambient の経路（ztunnel だけ・waypoint 経由）で、最大 rps・p99・CPU・RSS を比べる。
  1 ノードあたりの RSS も比べる（Pod の数を 10・50・100 にして）。
- 接続あたりの syscall（`mesh-syscalls.sh`）: 名前空間をまたぐ syscall（`setns`）が、接続の経路に出ない。

## リスク

- ZDS とノードの代理の CA の要求は公開の安定した API ではない。Istio のバージョンで変わり得るので、対応するバージョンを
  明記し、フィクスチャをバージョンごとに分ける（`tests/fixtures/istio/<版>/`）。
- 本家の ztunnel は Rust で性能も高く、veil に置き換える利得は F-222 のサイドカーほど大きくない可能性がある。段階 2 の時点で
  計測し、利得が無ければ段階 3 以降の優先度を下げる（判断と数値は本チケットに記録する）。

## 関連

F-215、F-216、F-217、F-220、F-221、F-224（waypoint・HBONE）、`src/mesh/hbone.rs`、`src/mesh/peer.rs`、`src/mesh/xds/`。
