# F-176: QEMU の BSD スクリプトで NetBSD を FreeBSD/OpenBSD 並みに扱う

**状態: 完了（feat/v080-limitations）**

## ギャップ（v0.7.0 の検証で実際に踏んだもの）

1. NetBSD x86_64 のコンソールがシリアルに出ない（`/boot.cfg` の `consdev=com0` が効いていない）
2. 異常終了後の fsck を自動で回復できない（全 OS 共通だが NetBSD x86_64 は 1 のため手作業が重い）
3. 既定 4GB で単体テストのリンクが OOM（`ld` が Killed、VM ごと落ちることもある）
4. NetBSD aarch64 の E2E が HTTP/3 抜き（B-62: libc の `_ALIGNBYTES` 誤りでテストクライアントの quinn-udp が panic）
5. 計測ハーネス（`tools/perf/freebsd/`）が FreeBSD 専用
6. `grow` が FreeBSD 専用
7. FreeBSD の capmode E2E に相当する OS 固有のセキュリティ検証が無い

## 改修

1. provision で `installboot` のコンソール指定と `/etc/ttys` を設定し、再起動後にシリアルの `login:` を確かめる。
   `screen`（QMP screendump）/ `sendkeys` コマンドを追加
2. NetBSD の `/` と `/work` を `log`（WAPBL）でマウント。全 OS 共通の `rescue`（シリアルでシングルユーザーを検出して fsck）
3. `VM_MEM_MB` の既定を OS/アーキ別に（NetBSD x86_64 = 7168）。単体テストのリンクを直列化
4. テストクライアント側で cmsg のアラインメントを正しく扱い、`full-netbsd-no-http3` への切り替えを廃止
5. `tools/perf/bsd/` に一般化（NetBSD/OpenBSD の nginx・syscall 計数・CPU プロファイル）
6. NetBSD aarch64 の `grow`
7. `bsd-security-e2e.sh`（FreeBSD capmode / NetBSD chroot + 特権降格 / OpenBSD pledge+unveil）

## 検証結果（2026-10-09〜10、feat/v080-limitations）

| 環境 | provision | unit | e2e | build | security-e2e |
|------|-----------|------|-----|-------|--------------|
| NetBSD 10.1 x86_64（KVM） | OK（`SERIAL_CONSOLE_OK`、`/`・`/work` は `log`） | 941 + 54 | 564 / 1 失敗（下記） | OK | PASS（chroot + nobody へ降格・静的配信・証明書リロード） |
| NetBSD 10.1 aarch64（HVF） | OK（作り直しで確認） | — | **565 / 0**（HTTP/3 込み。B-62 解消） | OK | PASS |
| FreeBSD 14.3 aarch64（HVF） | — | — | **565 / 0** | OK | PASS（capsicum capability mode・auto_reload で証明書リロード） |
| OpenBSD 7.9 x86_64（KVM） | — | — | **564 / 0 / 1 ignored**（B-71 の 4 件も成功） | OK | PASS（pledge + unveil） |
| OpenBSD 7.9 aarch64（HVF） | — | — | **564 / 0 / 1 ignored**（B-58） | OK | PASS（pledge + unveil） |

NetBSD x86_64 の 1 件は `test_http3_wasm_local_response`（WAF モジュールの初回要求での正規表現構築が
Pulley 実行で実行予算を超えて trap し、fail-open で 200）。NetBSD は常に Pulley（B-55）で、B-58 と同種の
負荷依存の現象（2026-08 の同環境は 10〜12 件失敗だった）。**単独実行では 3 回とも成功**（1.24〜1.38 秒）。

### サンドボックス E2E で見つかった不具合

- B-108: chroot 後に降格先のグループ名を引けず起動失敗 → 名前を chroot 前に ID へ解決
- B-109: chroot 下で静的配信がすべて 403 → chroot 直後にルートの canonical なベースパスを解決し直す
- FreeBSD capability mode では SIGHUP の設定ファイル再読込が ECAPMODE で失敗する（証明書は auto_reload で
  リロードできる。running.md に明記）

### 検証環境側で直したもの

- OpenBSD の LibreSSL は拡張なしの `req -x509` で X.509 v1 を作り rustls が拒否 → ゲストの証明書を v3 に
- NetBSD 同梱 GNU ld のデバッグ情報付きリンクが極端に遅い（x86_64 の E2E で veil 本体が 1 時間超）
  → OpenBSD と同じく `CARGO_PROFILE_DEV_DEBUG=0` 等
- macOS ホストで slirp の IPv6 UDP の `sendto(2)` がブロックして QEMU ごと停止 → `ipv6=off`
- quinn-udp の vendoring で import 削除時に cfg 属性が残り OpenBSD/macOS でテストクライアントが
  コンパイル不能（3a224f6 で修正）
