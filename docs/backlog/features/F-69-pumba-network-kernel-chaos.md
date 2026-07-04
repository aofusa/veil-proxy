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

## 残件

- `reorder`（順序逆転）シナリオの追加、複合障害（loss+delay 同時）。
- 実行して顕在化した回復不全があれば bugs/ に起票（本 PR 時点では未実行）。

## 受け入れ条件

- `SKIP_PUMBA=0` で各 netem シナリオを適用でき、適用解除後に Veil のヘルスが復帰すること。
