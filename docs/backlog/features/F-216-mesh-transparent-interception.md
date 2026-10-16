# F-216: メッシュの透過転送（SO_ORIGINAL_DST・仮想 inbound / outbound）とプロトコル検出

**状態: 完了（feat/mesh-dataplane-impl、2026-10-15）**

## 目的

Istio・Linkerd の init コンテナ / CNI が設定する iptables の REDIRECT（Pod の出入りの TCP を全部プロキシのポートへ曲げる）で
来た接続を、元の宛先ごとに正しく扱う。今の veil はリスナーのアドレスで受けた接続を設定のルートで振り分けるだけで、
元の宛先を見ていない（`SO_ORIGINAL_DST` の利用は無い）。

| | Istio | Linkerd |
|---|---|---|
| outbound（アプリ → 外） | 15001 | 4140 |
| inbound（外 → アプリ） | 15006 | 4143 |
| 除外する UID（プロキシ自身の通信を曲げない） | 1337 | 2102 |
| 管理・健康 | 15000（Envoy 管理）・15021（健康）・15090（Prometheus） | 4191（管理）・4190（制御・tap） |

## 設計

- **元の宛先の取得**: accept の直後に `getsockopt(SOL_IP, SO_ORIGINAL_DST)`（IPv6 は `SOL_IPV6, IP6T_SO_ORIGINAL_DST`）。
  接続ごとに 1 本（Envoy の `original_dst` リスナーフィルタと同じ）。io_uring・reactor の両方で、accept の戻り値の fd に対して
  同期の `getsockopt` を呼ぶ（ブロックしない syscall。新しい io_uring オペコードは足さない）。`reactor/tcp/` に足すときは
  `unix.rs` と `windows.rs` の両方（windows は未対応のエラー。B-69）。
- **仮想リスナー**: `Listener { kind: VirtualOutbound | VirtualInbound | Explicit }`（F-218 のメッシュモデル）。
  仮想 outbound は元の宛先 (IP, port) → フィルタチェーン（無ければ passthrough クラスタ = 元の宛先へそのまま L4 転送）。
  仮想 inbound は元の宛先のポート → アプリへの転送（Pod の IP か 127.0.0.1。Istio は既定で Pod の IP へ送る）。
- **ループの防止**: プロキシ自身の上流接続は UID の除外で曲がらない。veil は該当 UID で動く（特権降格の先を合わせる）。
  元の宛先が自分の仮想リスナーのアドレスだったら拒否する（設定ミスでの無限ループ防止）。
- **プロトコル検出**: 最初のバイトを読んで分類する（`proxy.rs` の `detect_protocol_with_buffer` を一般化）。
  - TLS の ClientHello（SNI・ALPN を取り出す。Istio の ALPN `istio-peer-exchange`・`istio-http/1.1`・`istio-h2`・`istio`、
    Linkerd の `transport.l5d.io/v1`）→ F-217（終端）か L4 の passthrough（SNI で振り分け。L4 の SNI ルーティングもここで実装）
  - HTTP/2 のプリフェース → HTTP/2、HTTP/1.x のリクエスト行 → HTTP/1.1、どれでもない → opaque（L4）
  - サーバが先に話すプロトコル（MySQL・SMTP 等）は待っても何も来ないので、検出のタイムアウト（Istio は
    `protocolDetectionTimeout`、Linkerd は 10 秒）か宛先ポートの宣言（Istio のポート名・`appProtocol`、Linkerd の
    opaque ports・skip ports）で L4 にする。
  - 読んだバイトは捨てずにそのまま次の段（HTTP パーサ・TLS・L4 の転送）へ渡す（読み直し・`MSG_PEEK` をしない）。
- **L4 の経路**: 今の `l4/` の双方向転送（splice）を、元の宛先・クラスタ単位で使えるようにする（今は `[[l4]]` の静的な
  リスナーだけ）。
- **TPROXY**（Istio の `interceptionMode: TPROXY`、送信元 IP を保つ）は `IP_TRANSPARENT` と `CAP_NET_ADMIN` が要るので後回し
  （記録のみ）。

## 受け入れ条件

- （必須、F-215）既存のプロキシの性能が退行しない（`tools/perf/suite_ab.sh` の main との交互 A/B、比の中央値 0.98 以上・0.95 未満なし）。
  コンテナで試すときのイメージは musl 版。

- 単体: `SO_ORIGINAL_DST` の取得（network namespace + iptables を作れる環境では統合試験、無ければスキップ）、プロトコル検出の
  分類表（分割された ClientHello・途中で切れたプリフェース・サーバが先に話す場合のタイムアウト）。
- E2E（Linux、`unshare -n` の network namespace で iptables REDIRECT を張る）: outbound の HTTP/1.1・h2c・TLS（passthrough）・
  opaque TCP がそれぞれ元の宛先へ届く。inbound がアプリへ届く。UID の除外でループしない。
- 接続あたりの syscall 増が `getsockopt` 1 本だけであることを strace で確認。io_uring・epoll の両方の E2E。


## 実装の結果

- virtualOutbound / virtualInbound・元の宛先のリスナーへの受け渡し・プロトコル検出（TLS の SNI/ALPN・h2c・HTTP/1・opaque。
  読んだバイトは再利用）。Istio の 127.0.0.6 からの inbound、UID 1337 の除外（istio-init / linkerd-init の iptables）。
- 接続あたりの syscall（`tools/k8s/mesh-syscalls.sh`。アイドルの回を差し引く）: 透過転送で増えるのは SO_ORIGINAL_DST の
  `getsockopt` 1 本だけ（以前は `getsockname` も 1 本あった→修正）。
- io_uring と epoll: epoll 版（既定のイメージ）で全スイート、io_uring 版で Istio の smoke 26/26・tasks 18/18。Linkerd は
  注入がプロキシに `seccompProfile: RuntimeDefault` を付けるので io_uring が使えない（理由を出して起動時に終わる）。
- `unshare -n` の network namespace での E2E はこのホストで作れない（非特権）。代わりに kind の Pod の中で istio-init /
  linkerd-init が張る iptables の上で、outbound の HTTP/1.1・h2c・TLS（passthrough）・opaque TCP、inbound を確かめた
  （mesh-e2e.sh の smoke、linkerd-e2e.sh の smoke・tasks）。

## 関連

F-215、F-218（リスナーの表）、F-217（TLS の終端）、`docs/guide/l4-proxy.md`（SNI ルーティングは未実装と書いてある箇所を更新）。
