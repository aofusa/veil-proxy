# F-230: メッシュの試験の残り（kind を使わない透過転送の E2E・ambient の bookinfo）

**状態: 未着手（2026-10-16 起票・設計。親 F-215・F-225 の残件）**

## 目的

F-215 の実装で、設計の通りにはできなかった試験が 2 つある。

1. **透過転送の E2E を network namespace で回す。** F-216 の設計では `unshare -n` で行う予定だった。このホストは非特権の
   ユーザー名前空間を作れない（Ubuntu 24.04 の `kernel.apparmor_restrict_unprivileged_userns = 1`）ので、kind の Pod の上の
   iptables で代わりに確かめた。kind の試験は 1 回 15 分以上かかり、変更のたびには回せない。
2. **ambient の waypoint を bookinfo で試す。** F-224 の試験は whoami で行った。複数のサービスの間の L7（reviews の版の振り分け・
   productpage から先の認可）と、観測（Kiali の辺・トレース）を、ambient で確かめていない。

## 設計

### 1. docker のコンテナの中の network namespace で回す透過転送の E2E（`tests/mesh_netns_e2e.sh`）

- 特権の docker のコンテナ（`--cap-add NET_ADMIN --cap-add SYS_ADMIN`）の中で `ip netns` を使う。docker は既にあり、
  ホストのユーザー名前空間の制限を受けない。
  - 名前空間を 2 つ作る（client・server）。veth でつなぎ、IPv4 と IPv6 の両方のアドレスを付ける。
  - client 側に、本家の iptables の規則をそのまま張る。
    - Istio: proxyv2 のイメージの `pilot-agent istio-iptables`、UID 1337 の除外、15001・15006。
    - Linkerd: `linkerd2-proxy-init`（4140・4143、UID 2102）。
  - 本家と同じ規則にすることで、捕捉の条件を自分で書き写した偽の合格を避ける。
- 中の veil は制御プレーン無しで動かす: Envoy のモードで、`static_resources` のブートストラップを使う
  （`src/mesh/envoy/bootstrap.rs` は対応済み）。リスナー・クラスタの記述は、kind で記録した istiod のフィクスチャから
  最小の組を作る。
- 確かめる組み合わせ:
  - outbound: HTTP/1.1・h2c・TLS（passthrough）・opaque TCP。
  - inbound: 127.0.0.6 から。
  - UID の除外によるループ防止。
  - `SO_ORIGINAL_DST` と IPv6 の `IP6T_SO_ORIGINAL_DST`。
  - 送信元 IP の保持。
  - プロトコル検出の読み残し（少しずつ届く ClientHello）。
  - 接続あたりの syscall（strace で数え、`getsockopt` 1 本だけ）。
- 実行の形: `tests/e2e_setup.sh` と同じく自動でセットアップ・後片付けし、数分で終わるようにする。`--features
  mesh-istio,mesh-linkerd,epoll` と io_uring 版の両方を回す。`docs/guide/testing.md`（英日）に入口を書く。
- `unshare -rn` で動く環境（制限の無いホスト）では、docker 無しでも回せるようにする。最初にユーザー名前空間を作れるかを
  確かめ、作れなければ docker の経路にする。

### 2. ambient の bookinfo（`tools/k8s/mesh-e2e.sh` の `ambient` の組に足す）

- ambient のプロファイルで bookinfo を `istio.io/dataplane-mode=ambient` にする。名前空間の waypoint を veil
  （Gateway の `infrastructure.parametersRef` でイメージを指定、F-224 と同じ）と本家の 2 通りで動かす。
- 確かめる組み合わせ（Istio の ambient のタスクに合わせる）:
  - productpage の応答。
  - waypoint の HTTPRoute での reviews の重み付きの移行（v1 → v3）。
  - L7 の AuthorizationPolicy（productpage からの GET だけ許可）。
  - L4 の AuthorizationPolicy（ztunnel で評価される。ztunnel は本家）。
  - Kiali の辺（waypoint を経由する辺と mTLS の鍵マーク）。
  - Jaeger のトレース（waypoint のスパン）。
  - waypoint の再起動の最中の成功率。
- F-228（ztunnel の置き換え）の試験も、この bookinfo の組の上に足す。

## 受け入れ条件

- `tests/mesh_netns_e2e.sh` が、このホストで stock（本家の Envoy をコンテナに入れた比較用）と veil の両方で通る。
  - 本家で通ることを先に確かめる（試験そのものの正しさ）。
  - 所要時間を `docs/guide/testing.md` に書く。
- `mesh-e2e.sh <stock|veil|mixed> ambient` の bookinfo の項目が全部通る（stock で先に通す）。
- （必須、F-215）既存のプロキシの性能が退行しない（試験の追加だけで本体のコードを変える場合も確かめる）。イメージは musl 版。

## 関連

F-215、F-216、F-224、F-225、F-228、`tools/k8s/mesh-e2e.sh`。
