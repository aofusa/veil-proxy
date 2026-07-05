# F-69: ネットワーク/カーネル層カオス（Pumba / tc netem）

出典: `security_chaos_fuzzing_report.md` §2.2.1。親: [F-53](F-53-chaos-engineering-expansion.md)。

## 目的

Toxiproxy（L7 寄り）より低レイヤーの障害（パケットロス・遅延・順序逆転・重複・破損）を
`tc netem` で直接注入し、Veil の耐性と回復性を検証する。

## 実装済み

- `tools/container_security/chaos/pumba_chaos.sh`（docker `gaiaadm/pumba` + docker.sock、
  既定 `SKIP_PUMBA=1`）。
  - 稼働中 Veil コンテナへ netem を時限適用: `loss 10%` → `delay 100ms±20ms` →
    `duplicate 5%` → `corrupt 2%` を順に。
  - 各シナリオで適用中/回復後のヘルスを確認し、復帰しなければ警告（backlog 起票対象）。
- `run.sh` に配線（`report.sh` に `pumba` 追加）。

## 実装済み（追加・2026-07-05）

- **`reorder`（順序逆転）シナリオ**を netem ループへ追加
  （`netem delay --time 40 reorder --percent 30 --correlation 50`）。
- **複合障害（loss+delay 同時）**: pumba の netem サブコマンドは 1 回 1 impairment
  （tc qdisc 単一で後勝ち置換）のため、対象コンテナの netns へ直接
  `tc qdisc add ... netem delay 80ms loss 8%` を適用する compound ブロックを追加
  （`--net container:<veil> --cap-add NET_ADMIN`、`IPROUTE2_IMAGE` 可変、
  `SKIP_PUMBA_COMPOUND=1` で無効化、tc 適用不可時はスキップ扱いで suite 継続）。

## 残件

- 実行して顕在化した回復不全があれば bugs/ に起票（docker 環境での実行が前提）。

## 受け入れ条件

- `SKIP_PUMBA=0` で各 netem シナリオを適用でき、適用解除後に Veil のヘルスが復帰すること。
