# B-330: v0.11.0-rc.2 の release で、glibc のイメージの照合・Windows の試験・OpenBSD の port の試験が落ちる

**状態: 修正済み（v0.11.0）**

## 事象と原因

v0.11.0-rc.2 の release（run 42193808705）で、配布物そのものではなく照合・試験の側が落ちた（FreeBSD は B-329）。

1. `image test glibc/amd64`・`glibc/arm64`: grype が Wolfi の `glibc-2.44` の CVE-2026-84243（High。gettext が環境変数
   `LANGUAGE` の `/`・`..` を拒まず任意の .mo を読ませる）を検出。2026-11-06 に DB に載り、Wolfi に直した版が無い
   （最新の 2.44-r13 も該当）。B-158 の方針で直す版の無いものも失敗にしている。
2. `dist / test Windows x86_64`: runner のイメージ（windows-2022 の 20261101）で Docker が動いておらず、Nano Server で
   起動する確認（B-167）が `docker_engine` に繋がらずに落ちた。veil.exe 自体は C ランタイムを import していない
   （cross の job の `check-pe-imports.py` は通過）。
3. `dist / test Windows aarch64`（x86_64 も同じ。2 で手前で止まっていた）: MSI の試験（F-290）が「インストールされている
   アプリ」のアイコンを Uninstall キーの `DisplayIcon` で見ていた。Windows Installer は `ARPPRODUCTICON` を
   `DisplayIcon` に書かず `HKCR\Installer\Products\<製品>\ProductIcon` に書く（aofish の MSI 製品 84 件で `DisplayIcon` は 0 件）。
   F-290 で足した試験が release で初めて走った。
4. `dist / port openbsd`: F-290 で hicolor のアイコンを入れたので port の実行時の依存に `x11/gtk+4,-guic` を足したが、
   試験の `pkg_add` がローカルのパッケージだけを見て依存を解決できなかった。

## 修正

1. `.grype.yaml` に理由付きの除外（CVE-2026-84243、パッケージ `glibc-2.44` に限る）。veil のイメージでは届かない:
   veil は gettext を呼ばない、ロケールは POSIX だけ（`LANGUAGE` は LC_MESSAGES が C/POSIX 以外でしか使われない）、
   シェルも環境変数を外から渡す経路も無い。Wolfi に直った版が出たら除外を消す。あわせて glibc-dynamic の
   ダイジェストを最新（glibc 2.44-r13）にした（`Dockerfile.glibc`・`.glibc.aarch64`・`.vds-server`）。
   2026-11-14: Wolfi が 2.44-r16 で直したので、ダイジェストを 2.44-r19 のものへ上げて除外を消した（B-400）。
2. Docker が動いていない runner では Nano Server の確認を notice を出して飛ばす（`check-pe-imports.py` が必ず見る）。
3. `tools/release/test-msi.ps1` は `ProductIcon` を見る。
4. `tools/release/bsd-port-test.sh` の OpenBSD の `pkg_add` に `PKG_PATH`（ミラー）を渡す。
