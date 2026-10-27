# B-241: nightly の `sanitizers` ジョブは `RUN_E2E_ASAN=1` と `RUN_E2E_TSAN=1` を同時に渡すが、`e2e_sanitizer_chaos.sh` は TSAN を優先して ASAN の E2E を一度も走らせていない

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 に aofish の Docker Desktop で `container-security-nightly` の `sanitizers` ジョブと同じ環境変数で回した
`tools/container_security/run.sh`（`docs/artifacts/security/scans/aofish-sanitizers/`、git 管理外）。`suite-sanitizers.log` に
`e2e_sanitizer start sanitizer=thread` は 1 回だけで、`sanitizer=address` の E2E は無い（`libfuzzer_asan` は走っている）。

## 事象

- `.github/workflows/container-security-nightly.yml` の `sanitizers` ジョブは `RUN_E2E_ASAN: "1"` と `RUN_E2E_TSAN: "1"` を同時に設定して
  `run.sh` を 1 回呼ぶ（feat/v0100 でも同じ）。
- `tools/container_security/chaos/e2e_sanitizer_chaos.sh` は「`RUN_E2E_TSAN=1` なら thread、そうでなければ address」の二者択一で、
  1 回の実行で 1 つしか回さない。両方 1 だと **thread だけ**走り、`e2e_sanitizer_report.txt` は thread の結果で終わる。
- ジョブ名・コメント（「ASAN/TSAN の E2E カオス」）と `suite_summary.json`（`e2e_sanitizer` は 1 フェーズ）からは分からない。
  B-116（LeakSanitizer が走っていなかった）と同じ「走ったつもり」。

## 影響

B-223〜B-225（終了時の use-after-free）のような ASAN でしか見つからない欠陥を、毎週のジョブが見ていない。

## 改修案

- `e2e_sanitizer_chaos.sh` を、有効な sanitizer を**順に全部**回す形にする（address → thread）。レポートは `e2e_sanitizer_<san>_report.txt` に分け、
  `lib/report.sh` の集計も 2 フェーズにする。
- または workflow の `sanitizers` ジョブを matrix（`sanitizer: [address, thread]`）にして 1 回 1 種類にする。
- F-279 の「graceful shutdown 中の ASAN」はこの修正の後に足す。

## 受け入れ条件

- `sanitizers` ジョブの artifact に address と thread の両方の `e2e_sanitizer` レポートがあり、Job Summary に両方の結果が出る。

## 修正（v0.10.0）

- `e2e_sanitizer_chaos.sh`: `RUN_E2E_ASAN=1` と `RUN_E2E_TSAN=1` が両方なら、自分を address → thread の順に別々に呼ぶ（それぞれ `e2e_sanitizer_<sanitizer>_report.txt`、`e2e_sanitizer_report.txt` には両方を続けて書く）。どちらかが失敗すれば非 0（`E2E_SANITIZER_BLOCKING=1` のとき）。workflow は変えていない。
- 確認: `docker` を常に失敗させる仮のコマンドで、両方の段が順に始まり 2 つのレポートができることを確かめた。実際の ASAN・TSAN は `container-security-nightly` の `sanitizers` ジョブで確かめる。
- `tools/container_security/README.md` に書いた。

## 関連

- [B-116](B-116-sanitizer-chaos-leak-check-never-ran.md)、B-217（集計の偽 OK。feat/v0100 で修正済み）、[F-279](../features/F-279-security-test-gaps.md)
