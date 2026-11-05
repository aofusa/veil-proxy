# B-292: release の NetBSD のビルドが時々 2 時間を超える（VM の中のコマンドが止まり、100 分の時間切れを待ってからやり直している）

**状態: 未修正（2026-11-05 起票。調査・設計のみ）**

## 事象

`release.yml` → `dist.yml` の `build netbsd x86_64` の job の時間（直近 13 回の release、`gh run view` の job の開始・終了）:

| 版 | 時間 | VM の step |
|---|---|---|
| v0.10.1 | 28m30s | 1 回で成功 |
| v0.10.1-rc.6 / rc.5 / rc.4 | 22m37s / 24m45s / 14m17s | 1 回で成功 |
| **v0.10.1-rc.3** | **2h07m30s** | **1 回目が 100 分の時間切れ → やり直しが 27 分で成功** |
| v0.10.1-rc.2 | 20m28s | 1 回で成功 |
| v0.10.0 / rc.2 / rc.1 | 25m17s / 24m10s / 19m08s | 1 回で成功 |
| v0.9.2 / rc.1 | 36m09s / 22m14s | 1 回で成功 |
| v0.9.1 | 28m48s | 1 回で成功 |
| **v0.9.0-rc.4** | **2h08m56s** | **1 回目が 100 分の時間切れ → やり直しが 28 分で成功** |

- 13 回中 2 回（約 15%）、NetBSD の VM の step が止まり、step の時間切れ（100 分）まで待ってから VM を作り直してやり直している。release の draft は
  dist の全 job を待つので、そのたびに release 全体が 2 時間を超える（ほかの job は長くて 37 分。v0.10.1 では `port openbsd`）。
- 止まらないときも NetBSD は FreeBSD（13〜18 分）より長い（20〜36 分）。OpenBSD（22〜30 分）と同程度。

## 調査メモ

- 2 回とも VM の起動は正常（`wait for ssh 7.30 s`）で、止まったのは `Running command`（`tools/release/bsd-guest-build.sh`）の中
  （rc.3: 10:02:57 に開始 → 11:42:38 に時間切れ。v0.9.0-rc.4: 22:27:06 → 00:06:47）。`dist.yml` のコメント「rc.2 の NetBSD は起動の途中で
  3 時間止まった」は、少なくともこの 2 回には当てはまらない。
- **どこで止まったかは分からない**: スクリプトの出力を VM の中の `out/build.log` へリダイレクトしており、job のログには何も出ない。時間切れで
  step が殺されると cross-platform-actions は `out/` をホストへ同期し直さず、さらに `stop the stuck VM` が `rm -rf out` するので、ログは残らない。
- `bsd-guest-build.sh` の NetBSD の段で、時間の上限が無いもの:
  1. `pkgin -y update`・`pkgin -y install`（`cdn.NetBSD.org` からの取得。libfetch に全体の時間切れが無く、相手が止まると待ち続け得る）
  2. `cargo build --locked --profile dist`（crates.io からの取得・fat LTO のリンク。NetBSD 同梱の GNU ld は遅い）
  3. `binary-smoke.sh`（veil を起動して curl。起動の待ちは 60 回 × 1 秒で上限があるが、**`curl` に `--max-time` が無い**ので、veil が
     接続を受けて応答しないと curl が永久に待つ）
  4. `cargo test --lib`（ローカルの模擬サーバを使う単体試験。NetBSD は拒否された connect で `connect_timeout` が Ok を返す
     [B-91](B-91-netbsd-connect-timeout-refused-ok.md)・[B-126](B-126-netbsd-connect-timeout-so-error.md) 等、待ちの挙動が他 OS と違う。
     試験が 1 つでも止まると `cargo test` 全体が終わらない）
- やり直しで通る（27〜28 分）ことから、決定的な不具合ではなく、ネットワークの取得か、負荷のかかった VM で止まる試験の可能性が高い。
  libtest は 60 秒を超えた試験を `test X has been running for over 60 seconds` と出すので、出力が見えれば 4 は特定できる。

## 改修案

1. **ログを job に流す**（原因の特定。まずこれ）: `run:` を `... 2>&1 | tee out/build.log` にし、終了コードはファイルに書いて拾う（BSD の `sh` に
   `pipefail` が無い OS があるので `{ sh build.sh; echo $? > out/rc; } 2>&1 | tee out/build.log`）。各段の開始に時刻を出す（`log()` に `date -u`）。
   時間切れのときも job のログに止まった段・60 秒を超えた試験の名前が残る。
2. **段ごとの時間の上限**（`timeout(1)`。NetBSD 7 以降・FreeBSD・OpenBSD 7 以降の base にある）: `pkgin` 10 分（失敗したら 1 回やり直す）、
   `cargo build` 45 分、smoke 5 分、`cargo test --lib` 25 分。単体試験が時間切れになったら、60 秒を超えていた試験の名前を拾って FLAKY の
   やり直し（今ある「落ちた試験だけ 1 回やり直す」）に乗せる。VM を作り直して最初からビルドし直すより速い（ビルド済みの試験バイナリを使える）。
3. **step の時間切れを縮める**: 普段の最長が 36 分なので、VM の step を 100 分 → 60 分。止まったときの最悪が 100 + 28 分 → 60 + 28 分になる
   （2 があれば、ほとんどは step の時間切れより前に段の時間切れで抜ける）。
   `binary-smoke.sh` の `curl` には `--max-time 10` を付ける（他 OS の smoke・`bsd-port-test.sh` の curl も同じに）。
4. **`stop the stuck VM` で `out/` を消す前に**、残っていればアップロードする（1 で job のログに出るので任意）。
5. （任意・計測してから）普段の時間を縮める: NetBSD の段の内訳を 1 の時刻で取り、長いのがリンクなら pkgsrc の `lld` を使う
   （`-C link-arg=-fuse-ld=lld`。NetBSD の GNU ld は遅い。memory の v0.8.0 の記録）。単体試験を別の job（別の VM）に分けて配布物のビルドと並べる案も
   ある（draft は両方を待つが、直列 → 並列になる）。
6. `dist.yml` のコメント（「起動の途中で止まった」）を調査結果に合わせて直す。

## 受け入れ条件

- 1〜3 を入れた release（rc でよい）で、NetBSD の job のログに各段の時刻が出る。止まった回があれば、どの段・どの試験かが job のログで分かり、
  その原因を別チケットで直す（試験が原因なら NetBSD での待ちの扱いを直す）。
- 段の時間切れで抜けたとき、job が失敗として報告され（`BUILD_FAILED`）、やり直しの step に進む。普段の時間は変わらない。

## 関連

- `.github/workflows/dist.yml`（`bsd` の job。f497282d で時間切れとやり直しを入れた）、`tools/release/bsd-guest-build.sh`、
  [B-110](B-110-netbsd-x86_64-wasm-waf-load-flaky.md)（NetBSD の負荷時だけの失敗）、[F-253](../features/F-253-bsd-ports.md)（`port netbsd` は別 job で 22〜27 分）
