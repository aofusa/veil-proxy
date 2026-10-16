# F-215: Kubernetes のサービスメッシュのデータプレーンを veil で置き換える（Istio の Envoy・Linkerd の linkerd2-proxy）

**状態: 完了（feat/mesh-dataplane-impl、2026-10-15。子 F-216〜F-225 完了）**

## 目的

Istio・Linkerd の制御プレーン（istiod・Linkerd の destination / identity / policy）は **そのまま**使い、
各 Pod に注入されるプロキシ（データプレーン）を veil に差し替える。利用者は Istio・Linkerd の API（VirtualService・
DestinationRule・AuthorizationPolicy・HTTPRoute・ServiceProfile・Server/AuthorizationPolicy 等）と運用ツール
（istioctl・Kiali・linkerd viz）をそのまま使い、プロキシだけが速く・軽くなる状態をゴールにする。

F-196（今のサイドカー）は「メッシュのプロキシの **隣**で veil がアプリの前段になる」構成で、メッシュのプロキシ自体は
残っている。本チケットはメッシュのプロキシそのものを置き換える。

### 前提の整理（置き換える対象）

| メッシュ | 注入されるプロキシ | veil が置き換えるもの | 制御プレーンとの契約 |
|---|---|---|---|
| Istio（サイドカー） | `istio-proxy` コンテナ = pilot-agent（Go）+ **Envoy** | Envoy のバイナリ（pilot-agent は残す） | Envoy の xDS v3（istiod → pilot-agent の xDS プロキシ → UDS → Envoy）。1.22 以降は Delta xDS が既定。証明書は pilot-agent の SDS（UDS） |
| Istio（ゲートウェイ・ambient の waypoint） | pilot-agent + **Envoy** | 同上 | 同上（HBONE の入出力が加わる） |
| Istio（ambient の ztunnel） | ztunnel（Rust、Envoy ではない） | 対象外（任意。F-224 で検討のみ） | Istio 独自の xDS 型（Address・Authorization） |
| Linkerd | **linkerd2-proxy**（Rust、**Envoy ではない**） | linkerd2-proxy 全体 | linkerd2-proxy-api の gRPC（Identity・Destination・InboundServerPolicies・OutboundPolicies）、環境変数 `LINKERD2_PROXY_*`、プロキシ同士の独自の取り決め（opaque transport・HTTP/1 → HTTP/2 の昇格） |

Linkerd は Envoy を使っていないので、「Envoy の置き換え」は Istio だけの話になる。本チケットでは依頼の意図を
「Istio・Linkerd のメッシュのデータプレーンを完全に veil にする」と解釈し、両方を対象にする。

### 「完全に置き換える」の定義（受け入れの基準）

1. **Istio**: 名前空間（またはクラスタ全体）のすべてのサイドカー・ingress / egress ゲートウェイ・waypoint が veil で、
   istiod は無改造。Istio の公式タスク（トラフィック管理・セキュリティ・可観測性）のうち EnvoyFilter を使わないものが
   全部通る。Gateway API の conformance（HTTPRoute・GRPCRoute のコア）が通る。istioctl の `proxy-status`・
   `proxy-config`（の主要な表示）・`analyze`、Kiali のグラフが動く。
2. **Linkerd**: 名前空間のすべての Pod のプロキシが veil で、Linkerd の制御プレーンは無改造。mTLS・プロトコル検出・
   opaque ports・HTTPRoute / GRPCRoute・認可ポリシー・リトライ・タイムアウト・サーキットブレーカ（failure accrual）・
   multicluster の gateway 無し構成（任意）が動き、`linkerd check --proxy`・`linkerd viz stat/top/edges`（tap は任意）が動く。
3. **混在**: 同じメッシュで veil の Pod と本家のプロキシ（Envoy・linkerd2-proxy）の Pod が相互に通信できる
   （移行は Pod 単位で段階的に行うので必須）。
4. **既存の性能の維持（必須）**: メッシュを使わない veil のプロキシの性能が退行しない（下記「必須」の節）。
5. **性能**: 同じクラスタ・同じ負荷で、本家のプロキシより追加レイテンシ（p50・p99）・CPU・メモリが小さい
   （数値目標は F-225 でベースラインを測ってから決める）。
6. **対象外（明示）**: Istio の EnvoyFilter（Envoy の内部構造へのパッチ。受け取ったら NACK して istiod の
   `proxy-status` に理由を出す）、Envoy の Lua フィルタ、Envoy 固有の統計名の完全互換（Istio の標準メトリクスは対象）。

## 必須: veil の既存のプロキシの性能を退行させない

メッシュの対応は **veil の既存のプロキシ（メッシュを使わない構成）の性能に退行を出さないことを必須条件**とする。

- メッシュの機能は `mesh*` feature とメッシュのリスナー（仮想 inbound / outbound・メッシュモデルを参照するリスナー）の中に閉じ、
  既存のリスナー・ルート・上流の経路には「メッシュの設定が無ければ何もしない」分岐（ロード時に決まる値の確認 1 回）以上を足さない。
  一般機能として既存の経路に入れるもの（mTLS の検証・L7 の新機能等）も、設定していないときのコストはゼロにする
  （ロード時に解決した `Option` の確認だけ。要求ごとの確保・ロック・syscall を増やさない）。
- 判定: 各子チケットの完了時とマージ前に、`tools/perf` の交互 A/B（`suite_ab.sh`、main とブランチ、io_uring と epoll、
  3B と 54KB、HTTP/1.1・HTTP/2・h2c・HTTP/3・gRPC の代表構成）で **比の中央値が 0.98 以上、どの構成も 0.95 未満にならない**
  （F-200・F-202・api-roadmap と同じ基準）。外れた構成は原因を潰すまでマージしない。結果は `docs/perf/README.md`。
- `default`・`full*` のバイナリに mesh のコード・proto の生成物が入らないことを検査する（`tests/api/library_e2e.sh nonregression` と同じ方式）。

## 全体設計

```
                       ┌──────────────── veil（mesh feature） ────────────────┐
istiod ─xDS─▶ pilot-agent ─UDS─▶ │ Istio アダプタ（F-222）   ┐                          │
                                 │  Envoy xDS v3 → メッシュモデルへ変換      │                          │
Linkerd 制御プレーン ─gRPC─────▶ │ Linkerd アダプタ（F-223） ┤→ メッシュモデル（F-218）─┼▶ データプレーン
                                 │  proxy-api → メッシュモデルへ変換        ┘  ArcSwap・リソース単位 │   透過転送（F-216）
                                 │ 制御プレーンとの通信は専用スレッド（ctrl_http）                    │   mTLS（F-217）
                                 └────────────────────────────────────────────────────────────────┘   L7（F-219）・認可（F-220）・観測（F-221）
```

### 1. メッシュモデル（制御プレーンに依存しない中間表現。F-218）

Envoy の xDS も linkerd2-proxy-api も、最終的には「どのポートで・どの接続を・どの相手として受け、どのルート表で・
どのクラスタの・どのエンドポイントへ・どの ID を期待して送るか」に落ちる。veil の中にこれを表す型を 1 つ作り、
アダプタはそこへ変換するだけにする（データプレーンはアダプタを知らない）。

- **Listener**（受け口）: 仮想 outbound（元の宛先で振り分け）・仮想 inbound・明示ポート。フィルタチェーンの一致条件は
  元の宛先 IP/ポート・SNI・ALPN・検出したプロトコル・送信元。
- **RouteTable**（HTTP のルート表）: 仮想ホスト（Host・`:authority`）→ ルート（パス・ヘッダ・メソッド・クエリ）→
  アクション（重み付きのクラスタ・リダイレクト・直接応答・リトライ・タイムアウト・ミラー・フォルト・ヘッダ操作）。
- **Cluster**（宛先のまとまり）: LB・接続プール・外れ値検出・サーキットブレーカ・上流 TLS（期待する ID）・プロトコル。
- **EndpointSet**: エンドポイント（アドレス・重み・ロカリティ・健康状態・メトリクスのラベル・期待する ID・
  プロトコルのヒント）。
- **Policy**: 認可（主体・名前空間・IP・ポート・パス・メソッド・ヘッダ・JWT の claim）と認証（JWT）。
- **Identity**: 自分の証明書・鍵・信頼アンカー（F-217）。

今の veil の設定（TOML → `Route`・`[upstreams]`）とは別の型にする。理由: メッシュの更新は頻度が高く（エンドポイントの
入れ替え）規模が大きい（数千サービス）ので、TOML 合成 → 全体の検証 → 全体の ArcSwap（`config_override` の経路）を
更新ごとに通せない。リソースごとに `ArcSwap` を持ち、エンドポイントの更新はそのクラスタの `EndpointSet` の差し替えだけで
終える（LB の状態・ヘルスチェッカーはクラスタにつき 1 個で引き継ぐ。F-156）。F-211 の 4（EDS 相当）・5（Delta）・
6（ロカリティ）はメッシュモデルの上で実装し、VDS もこの型へ変換して共有する。

### 2. データプレーン（F-216・F-217・F-219〜F-221）

- **透過転送**（F-216）: iptables の REDIRECT（Istio・Linkerd の init コンテナ / CNI が設定）で来た接続の元の宛先を
  `SO_ORIGINAL_DST` で取り、仮想 outbound / inbound のリスナーで受ける。プロトコル検出（TLS ClientHello・HTTP/2 の
  プリフェース・HTTP/1.x）で HTTP の経路と L4 の経路へ分ける。
- **ワークロード ID の mTLS**（F-217）: 受け側のクライアント証明書の検証（Istio は SPIFFE の URI SAN、Linkerd は DNS SAN）、
  送り側のクライアント証明書と相手の ID の検証、証明書のメモリ上の差し替え、Istio の PERMISSIVE（同じポートで mTLS と平文）。
- **L7 のトラフィック制御**（F-219）・**認可**（F-220）・**観測**（F-221: Istio・Linkerd の標準メトリクスと分散トレース）。

### 3. アダプタ（F-222・F-223・F-224）

- **Istio**（F-222）: pilot-agent は残し、`ProxyConfig.binaryPath`（既定 `/usr/local/bin/envoy`）の位置に veil を置いた
  イメージを `sidecar.istio.io/proxyImage`（または注入テンプレート）で注入する。veil は Envoy のコマンドライン（pilot-agent が
  渡す範囲）とブートストラップ JSON を読み、xDS（ADS、Delta / SotW）を UDS で受けてメッシュモデルへ変換する。
  証明書は SDS（UDS）。pilot-agent が Envoy に期待する管理 API（`/ready`・`/stats` の一部・`/drain_listeners`・
  `/quitquitquit`・`/config_dump`）を模す。
- **Linkerd**（F-223）: linkerd2-proxy のイメージの代わりに veil を `config.linkerd.io/proxy-image` で注入する。
  環境変数の設定を読み、Identity（CSR → 証明書）・Destination・Policy の gRPC を購読する。プロキシ同士の取り決め
  （opaque transport・`l5d-orig-proto` の HTTP/2 昇格・`l5d-dst-*` 等）と管理ポート（4191 の `/metrics`・`/ready`・`/live`）を実装する。
- **Istio のゲートウェイ・ambient**（F-224）: ingress / egress ゲートウェイ（F-222 と同じ変換、透過転送無し）、
  waypoint（HBONE = mTLS の HTTP/2 CONNECT の受け・送り）。ztunnel は検討のみ。

### 4. ビルドと配布

- feature: `mesh`（メッシュモデル・データプレーンの追加機能）・`mesh-istio`（Envoy の xDS の proto と変換、管理 API の模倣）・
  `mesh-linkerd`（linkerd2-proxy-api の proto と変換）。**default・`full*` には入れない**（proto の生成物が大きい。
  ライブラリの feature と同じ扱いで、`tests/api/library_e2e.sh nonregression` と同様に full バイナリに入っていないことを検査する）。
  データプレーン側の追加（SO_ORIGINAL_DST・mTLS の検証等）は `mesh` 無しでも使える一般機能として `full` に入れてよい。
- Linux のみ（Kubernetes のノード）。バックエンドは epoll を基本にする: containerd / Docker の `RuntimeDefault` の seccomp は
  io_uring を拒否するため（F-196 と同じ `full-container` 系）。`Localhost` の seccomp プロファイルを配れる環境向けに
  io_uring 版も測る。
- **イメージは musl 版**（`docker/Dockerfile.musl` の静的リンクの veil。F-196 と同じく musl をメインにする）。
  `docker/Dockerfile.mesh-istio` は musl の veil に、`istio/proxyv2` から pilot-agent（Go の静的バイナリ）と必要な
  ファイル（ブートストラップのテンプレート等）だけをコピーした distroless のイメージ（Envoy は入れない）。
  `docker/Dockerfile.mesh-linkerd` は musl の veil 単体（UID 2102）。Istio の UID 1337・Linkerd の UID 2102 は
  iptables の除外条件なので、その UID で動かす（veil の特権降格の先も同じ UID）。
- proto は `third_party/` に必要な部分だけ置き（Envoy の data-plane-api・xds・udpa・Istio の proto・linkerd2-proxy-api、いずれも
  Apache-2.0。LICENSE と取得元のコミットを README で記録）、`veil-build`（F-208）の prost 生成を使う。

## ホットパス・設計制約との整合

- 制御プレーンとの通信（xDS・SDS・Identity・Destination・JWKS 取得）は専用スレッドの `ctrl_http`。データプレーンに触れない。
  変換・検証も専用スレッドで行い、データプレーンは ArcSwap の読み出しだけ（接続ごとにリスナーの表を 1 回、要求ごとにルート表を 1 回）。
- 接続ごとの追加 syscall は `getsockopt(SO_ORIGINAL_DST)` 1 本（Envoy と同じ）。新しい io_uring オペコードは足さない。
  プロトコル検出は受信バッファを読んでそのまま再利用する（`MSG_PEEK` を使わない、読み直さない）。
- メトリクスのラベル（送信元・宛先のワークロード等）は接続の確立時（相手の ID が分かった時点）に解決し、要求ごとは
  事前確保したカウンタのアトミック加算だけ（要求ごとの `String`・`HashMap` 禁止）。
- 証明書の差し替えは F-105 の世代ゲートと同じ考え方（鍵はメモリのみ・zeroize）。
- サンドボックス: seccomp の許可は既存の範囲で足りるか F-216 で確認（`getsockopt` は許可済みの想定）。Landlock は SDS の UDS・
  サービスアカウントのトークンのパスを許可に足す。

## 実施順（推奨）

| 段 | チケット | 内容 | 理由 |
|---|---|---|---|
| 0 | F-225 | kind の計測・試験基盤と Envoy・linkerd2-proxy のベースライン | 目標値と退行の判定を先に持つ |
| 1 | F-216・F-217・F-218 | 透過転送・mTLS・メッシュモデル | どちらのメッシュにも要る土台 |
| 2 | F-223 | Linkerd アダプタ | API が小さく閉じている（Envoy の xDS より面積が 1 桁小さい）。土台を端から端まで検証できる |
| 3 | F-222・F-219・F-220・F-221 | Istio のサイドカー | 主目的（Envoy の置き換え）。変換の面積が大きいので段階的に（下記） |
| 4 | F-224 | Istio のゲートウェイ・waypoint（・ztunnel の検討） | サイドカーの変換をそのまま使える |

Istio は istiod が生成する Envoy の設定（フィルタの種類は有限）を基準に段階を切る: (a) mTLS + 透過転送 + ルーティング
（VirtualService・DestinationRule・ServiceEntry）、(b) 認可・認証、(c) テレメトリ（Telemetry API・Kiali）、
(d) WasmPlugin（veil の Proxy-Wasm で動かす。ECDS）、(e) ゲートウェイ・waypoint。

## リスク

- **追従のコスト**: Istio・Linkerd の版ごとに生成される設定・API が変わる。対応する版を明記し（最初は Istio 1.31・Linkerd
  edge-26.10、F-196 と同じ）、kind の E2E を版の行列で回す。istiod が実際に生成した xDS を試験のフィクスチャとして保存し、
  変換の単体試験に使う。
- **Envoy の xDS の面積**: 未知の `typed_config` は、必須なら該当リスナー / ルートを NACK、`is_optional` なら無視して記録。
  黙って捨てない（ポリシーの抜けがセキュリティ事故になる。特に認可フィルタ）。
- **linkerd2-proxy-api は公開の安定 API ではない**: proto の版を固定し、Linkerd の版との対応表を持つ。
- **混在の互換**: 相互運用の試験（F-225）を各段の受け入れ条件に入れる。
- **規模**: 数千サービスのメッシュで Envoy は設定の大きさに比例してメモリを使う。メッシュモデルは共有できるもの
  （ルート表・クラスタ）をワーカー間で共有し、Sidecar リソース（Istio の可視範囲の絞り込み）を尊重する。

## 子チケット

| チケット | 内容 |
|---|---|
| F-216 | 透過転送（SO_ORIGINAL_DST・仮想 inbound / outbound）とプロトコル検出 |
| F-217 | ワークロード ID の mTLS（受け・送り・ID の検証・証明書の差し替え・PERMISSIVE） |
| F-218 | メッシュモデルとリソース単位の動的更新（F-211 の 4〜6 を含む） |
| F-219 | L7 のトラフィック制御の不足分（リトライ・タイムアウト・重み付き分割・ミラー・フォルト・ヘッダ操作・ロカリティ LB 等） |
| F-220 | メッシュの認可・認証（RBAC・JWT・ext_authz） |
| F-221 | メッシュの観測（Istio・Linkerd の標準メトリクス・分散トレース・アクセスログ） |
| F-222 | Istio のサイドカーのアダプタ（Envoy のブートストラップ・xDS v3 の変換・SDS・管理 API の模倣・イメージ） |
| F-223 | Linkerd のアダプタ（環境変数・Identity・Destination・Policy・プロキシ同士の取り決め・管理ポート） |
| F-224 | Istio のゲートウェイ・ambient（waypoint・HBONE、ztunnel は検討） |
| F-225 | メッシュの試験・計測基盤（kind、本家との混在、Gateway API conformance、Envoy・linkerd2-proxy との比較） |


## 実装の結果

- `src/mesh/` の専用エンジン（feature `mesh`・`mesh-istio`・`mesh-linkerd`、default・`full*` には入れない）。同じバイナリが
  `argv[0]` で Envoy（pilot-agent の `/usr/local/bin/envoy`）か linkerd2-proxy として起動する。
- イメージは musl 版の veil を本家のプロキシのイメージに入れたもの（`docker/Dockerfile.mesh-istio`・`Dockerfile.mesh-linkerd`）。
- kind の Istio 1.31.1・Linkerd edge-26.10.1 で、本家（stock）・veil・混在（mixed）の全スイートを通した（各子チケット）。
- 既存のプロキシの非退行: `tools/perf/suite_ab.sh` の main との交互 A/B（結果は docs/perf/README.md の「サービスメッシュ」と
  「非退行」の節）。
- 利用者向け: `docs/guide/mesh.md`（英日）。

## 関連

F-196（今のサイドカー構成）、F-211（VDS と xDS の比較。4 以降は F-218 に統合）、F-04・F-199（VDS）、F-105（証明書の
ホットリロード）、F-156（ワーカー間の共有状態）、F-208（veil-build）、Proxy-Wasm（`docs/guide/wasm.md`）。
