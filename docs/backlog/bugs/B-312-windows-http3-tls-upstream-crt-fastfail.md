# B-312: Windows で HTTP/3 の要求を TLS の上流へ中継するとプロセスが落ちる（CRT の `read`/`write` にソケットを渡す）

**状態: 修正済み（v0.11.0）**

## 事象

Windows（aofish）の E2E で、途中からプロキシ（8443）への接続がすべて拒否され 13 件が落ちた（B-311 の調査中に判明）。
プロキシはログを残さずに終了し、終了コードは `0xC0000409`（`__fastfail`）。WER のミニダンプでは例外のパラメータが
`5`（`FAST_FAIL_INVALID_ARG`）、場所は `ucrtbase.dll`、呼び出し元は `veil::http3_stream::raw_fd_write`
（`TlsBackend::flush_tls`）だった。curl の HTTP/3 で上流のある経路へ 1 回要求するだけで再現する（上流が無い 502 では起きない）。

## 原因

`src/http3_stream.rs` の `raw_fd_read`/`raw_fd_write`（HTTP/3 のストリーミング経路で TLS の上流と読み書きする）が
`cfg` なしで `libc::read`/`libc::write` を呼んでいた。Windows の `libc::read`/`write` は CRT の `_read`/`_write` で、
CRT のファイル記述子を取る。Winsock のソケットを渡すと CRT の不正引数ハンドラが呼ばれ、既定ではプロセスを `__fastfail`
で終了する（Rust の panic にならず、標準エラーにも何も出ない）。他の同種の関数（`simple_tls::raw_read` 等）は Windows 版を
持つか unix・Linux 限定だった。

## 修正

Windows では Winsock の `recv`/`send` を使う版を足した（`WSAEWOULDBLOCK` は std が `WouldBlock` にするので待ちの扱いは同じ）。

## 調べ方（Windows の無言の終了）

- PowerShell の `Start-Process -PassThru` で終了コードを取る（Git Bash の `$?` は 127 等に化ける）。
- `HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\veil.exe` にミニダンプを出させ、例外の記録と
  スタック上の `veil.exe` 内の戻り番地を拾って `llvm-symbolizer --obj=veil.exe`（同じ場所に `veil.pdb`）で引く。

## 関連

- B-311（Windows の E2E）
