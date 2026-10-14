# F-223: Linkerd のアダプタ（linkerd2-proxy の置き換え）

**状態: 未着手（2026-10-14 起票・設計。親 F-215）**

## 目的

Linkerd が注入する linkerd2-proxy（Rust、Envoy ではない）を veil に差し替える。Linkerd の制御プレーンは無改造。
注入は `config.linkerd.io/proxy-image`（と `proxy-version`）の注釈でイメージを差し替える（proxy-injector がそのまま注入する）。

## 設計

1. **設定**: proxy-injector が渡す環境変数 `LINKERD2_PROXY_*`（inbound / outbound / admin / control のアドレス、
   `..._DESTINATION_SVC_ADDR`・`..._IDENTITY_SVC_ADDR`・`..._POLICY_SVC_ADDR`、`..._IDENTITY_TRUST_ANCHORS`・
   `..._IDENTITY_LOCAL_NAME`・`..._IDENTITY_TOKEN_FILE`、`..._INBOUND_PORTS`・opaque ports・`..._INBOUND_DEFAULT_POLICY`、
   ログ、タイムアウト、`..._SHUTDOWN_ENDPOINT_ENABLED` 等）を読んで veil の設定とメッシュモデルの初期値を作る（`veil linkerd-compat`、
   またはイメージの ENTRYPOINT で自動判定）。対象の版の proxy-injector のテンプレートから一覧を確定する。
2. **Identity**: 起動時に鍵を作り（ECDSA P-256、メモリのみ）、CSR とサービスアカウントのトークン（projected volume）を
   `io.linkerd.proxy.identity.Identity/Certify` に送って証明書を受け取る。有効期限の前に更新（専用スレッド）。証明書が届くまで
   Ready にしない（Linkerd と同じ）。信頼アンカーは環境変数。F-217 の差し替えに渡す。
3. **Destination**: outbound の接続ごとに宛先（Service の authority か IP:port）を `Destination.Get` で購読し、エンドポイント
   （アドレス・重み・メトリクスのラベル・`TlsIdentity`・`ProtocolHint`（H2 = HTTP/2 へ昇格可・opaque transport の inbound ポート）・
   `authority_override`）を F-218 の `EndpointSet` にする。`GetProfile` で ServiceProfile（ルート・リトライ可否・タイムアウト・
   opaque かどうか）。購読は宛先ごとに共有し、使われなくなったら一定時間で解除する（多数の宛先で購読が溜まらないように）。
4. **Policy**: `InboundServerPolicies.WatchPort`（ポートごとの Server・認可・HTTPRoute / GRPCRoute・プロトコル）と
   `OutboundPolicies.Watch`（宛先ごとの HTTPRoute / GRPCRoute・バックエンド・リトライ・タイムアウト・failure accrual）を
   F-218・F-219・F-220 の型へ変換する。
5. **プロキシ同士の取り決め**（本家の linkerd2-proxy と混在させるために必須）:
   - mTLS: ALPN と SNI（相手の ID）を本家と同じに。ID は DNS SAN（F-217）。
   - **HTTP/1 → HTTP/2 の昇格**: 宛先が H2 のヒントを持つと、アプリの HTTP/1.1 を HTTP/2 でプロキシ間を運び、
     `l5d-orig-proto` ヘッダで元の版を伝え、受け側で HTTP/1.1 に戻す。
   - **opaque transport**: opaque な宛先へは ALPN `transport.l5d.io/v1` で TLS を張り、最初に TransportHeader（宛先ポート・名前・
     セッションのプロトコル。proto の長さ前置き）を送る。受け側は 4143 で受けて元のポートへ。
   - `l5d-dst-canonical`・`l5d-client-id`（受け側でアプリへ付ける）・`l5d-proxy-error`（プロキシが生成したエラーの印）等のヘッダ。
   - 対象の版の linkerd2-proxy のソースで取り決めの一覧を確定し、互換の試験（F-225）を先に書く。
6. **管理ポート 4191**: `/metrics`（F-221 の Linkerd のメトリクス）・`/ready`・`/live`・`/shutdown`（有効時）・`/env.json`（任意）。
   制御ポート 4190 の tap は任意（後回し）。
7. **起動順と終了**: native sidecar（`restartPolicy: Always` の init コンテナ）で、Ready（証明書が届いた）までアプリの起動を待たせる。
   `linkerd-await` を使うアプリのために `/ready` を本家と同じ意味にする。SIGTERM で排出（F-187）。

## 受け入れ条件

- （必須、F-215）既存のプロキシの性能が退行しない（`tools/perf/suite_ab.sh` の main との交互 A/B、比の中央値 0.98 以上・0.95 未満なし）。
  コンテナで試すときのイメージは musl 版。

- emojivoto・books を全 Pod veil で動かし、`linkerd check --proxy`（版の警告は許容）、`linkerd viz stat/edges/top` が動く。
- 本家の linkerd2-proxy の Pod と veil の Pod の相互通信（HTTP/1.1 の昇格・gRPC・opaque の TCP（例: Redis / MySQL）・mTLS の
  `client_id` の受け渡し）を両方向で確認。
- Linkerd のタスク（Automatic mTLS・Restricting Access To Services・Per-Route Policy・Retries・Timeouts・Traffic Split・
  Circuit Breaking（failure accrual））を F-225 の kind で通す。
- 証明書の更新（有効期限を短くした Linkerd で）・制御プレーンの再起動・Pod の SIGTERM。

## 関連

F-215、F-216〜F-221、F-196（今の Linkerd との共存構成）、F-225。
