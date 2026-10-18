# B-154: openat2 が seccomp で拒否される環境で、起動後最初の静的配信の要求が 404 になる

## 出典

v0.8.0 のリリース前の `tools/container_security/run.sh`（フル拡張）。ヘルスチェックが通らず
「Veil が起動しませんでした」で止まった。

## 事象

- `docker/assets/security/seccomp.json`（コンテナの seccomp プロファイル）に `openat2` が無く、
  `openat2(RESOLVE_BENEATH)` が `EPERM` になる。
- `cache` feature の静的配信（`cache::static_file::get_static_file_with_content`）は高速経路
  （`open_and_read_fast`）が `None` を返すと、それを 404 として返していた。openat2 の `EPERM` を
  最初に観測した要求では `resolve` が「以後フォールバック」と記録して `None` を返すため、
  **その 1 要求だけ** 404 になる（2 回目以降は高速経路を通らず canonicalize の経路で 200）。
- 再現: `veil:glibc` を上記プロファイルで起動し、`/`・`/index.html` などどれでも最初の 1 回が 404。
  `seccomp=unconfined` では起きない。

## 原因

`open_and_read_fast` の `None` が「404」と「高速経路で判定できない」の両方を表していた。
`resolve::open_beneath_in_root` / `open_beneath_for_request` の `None` は後者（呼び出し側は
canonicalize の経路へ進む）だが、`?` で 404 に変わっていた。

## 修正

- `FastOutcome::Unavailable` を足し、高速経路で判定できないときは canonicalize の経路
  （`file_cache::get_file_info_with_config` + 封じ込め検査）へ進む（単体試験
  `fast_path_without_registered_root_falls_back`）。
- コンテナの seccomp プロファイル（`seccomp.json`・`seccomp-epoll.json`）に `openat2` を足す
  （veil 自身の `ALLOWED_SYSCALLS` には F-153 から入っている。コンテナでは毎回フォールバックして
  readlink を連発していた）。

## あわせて直したもの（試験の不具合）

`tools/container_security` のハーネスは平文の HTTP を `:80` で叩いていたが、F-200 で試験用の設定の
平文リスナーは `listen_cleartext = 0.0.0.0:8443` だけになっていた（`:80` のリダイレクト専用リスナーは
無くなった）。ハーネスの `VEIL_HTTP_PORT` の既定を 8443 にした。

## ステータス

完了（v0.8.0）。
