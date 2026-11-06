# B-329: リリースの BSD の VM のビルドが、target をランナーへ同期し戻して失敗する

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0-rc.2 の release の `dist / build freebsd x86_64` が失敗した。VM の中のビルド・単体試験（1197 件）・起動の試験は
通ったが、step の後に cross-platform-actions が VM の作業ディレクトリを丸ごとランナーへ同期し戻すところで、

- root で作った `target/debug/incremental/*/*.lock` を読めず rsync が code 23、
- やり直しの step の同期でランナーの容量が尽きて（`No space left on device`）code 11、

となり、`out/build.log` が戻らず「no build log」で落ちた。単体試験の debug のビルドが大きくなり（v0.11.0 で C の kit 等が
増えた）、ランナーの空きを越えた。

## 修正

`tools/release/bsd-guest-build.sh` の最後で `target` を消す（配布物・ログは `$OUT` にある）。同期し戻すのは
ソースと `out/` だけになる。
