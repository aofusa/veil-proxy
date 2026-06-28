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
