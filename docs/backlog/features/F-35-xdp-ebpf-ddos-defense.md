# F-35: XDP/eBPF 最前線 DDoS 防御

## 出典

`docs/artifacts/architecture_analysis_v4.md` / `v5.md`（ネットワーク層の eBPF/XDP 最前線防御）。

## 概要

seccomp/Landlock は「カーネルをアプリから守る」防御であり、アプリ自身を外部の
DDoS / SYN Flood から守らない。XDP（eXpress Data Path）プログラムを NIC ドライバ段で
アタッチし、不正トラフィックを io_uring に到達する前にドロップする機能を設ける。

## 改修内容

1. `aya`（純 Rust eBPF）ベースの XDP プログラムを optional feature（例: `xdp`）として追加。
   - SYN Flood レート制限、ブロックリスト IP の早期ドロップ、簡易コネクションレート制限。
2. ユーザースペース側で BPF マップ（ブロックリスト/レート設定）を `config.toml` から更新できる
   コントロールプレーンを用意。
3. 非対応環境（XDP 不可・権限不足）では無効化し、既存のユーザースペースのレート制限/IP 制限に
   フォールバック。

## 受け入れ条件

- [ ] `xdp` feature 無効時は一切影響なし（`default = []` を壊さない）。
- [ ] 有効時、ブロックリスト IP のパケットが XDP 段でドロップされる。

## 依存・リスク

- `aya` 依存追加・CAP_NET_ADMIN/CAP_BPF 等の特権が必要。大規模機能のため独立 PR 推奨。
- データプレーンのホットパス外（NIC 段）の機能だが、運用統合（メトリクス・設定）が必要。

## 対応状況: ユーザースペース最前線防御を実装（XDP は環境制約で別途）

### 実装（検証可能なユーザースペース版）

XDP/eBPF 本体（NIC ドライバ段ドロップ）は **nightly + bpf-linker でのビルド**、実行に
**CAP_BPF/CAP_NET_ADMIN と XDP 対応 NIC** を要し、本リポジトリのサンドボックスでは
**ビルドも E2E 検証も不可**（`default = []` を壊さず、検証可能なものを優先する方針）。

そこで「不正トラフィックを高コスト処理の前に弾く」という本来の意図を、**検証可能な
ユーザースペース最前線防御**として実装した:

- `[security] blocked_ips`（CIDR リスト）を追加。`config.rs` で起動時・SIGHUP リロード時に
  `CidrRange` へパースし、ロックフリーな `ArcSwap<Vec<CidrRange>>`（`GLOBAL_BLOCKED_IPS`）に保持。
- `main.rs` の **3 つの accept ループ**（TLS / HTTP リダイレクト / H2C）で、accept 直後・
  **TLS ハンドシェイクおよびハンドラ spawn の前**に `is_ip_blocked(peer_addr.ip())` を評価し、
  マッチした接続を即時切断。`CidrRange::contains_addr(IpAddr)` を新設し、文字列化せずに
  **ゼロアロケーション**で判定（ブロックリスト空時は即 false でオーバーヘッドなし）。
- ルート単位の `denied_ips`（TLS/ルーティング後に評価）より **前段**で弾くため、既知の不正 IP に
  対する TLS ハンドシェイク等の高コスト処理を回避できる。
- `examples/config.toml` / `README` / `docs/readme/README.ja.md` に設定例とセマンティクスを追記。

### 検証

- 単体テスト 4 件追加（`config::blocklist_tests`: IPv4/IPv6 CIDR・単一 IP・グローバル設定/判定）通過。
- E2E（features full）回帰なし（デフォルトでブロックリスト空 = 無影響）。

### 残（XDP/eBPF 本体）

NIC ドライバ段でのドロップ（SYN Flood レート制限等）は、`aya` ベースの XDP プログラム
（別クレート・BPF ターゲットビルド）+ CAP_BPF + 対応 NIC を要し、専用環境での実装・検証が
必要。設計は本チケット上部のとおり。ユーザースペース最前線（accept 段）は本対応で実装済み。
