# B-260: サンドボックスの `/dev`（`sandbox_mount_dev`）で `/dev/null`・`/dev/zero`・`/dev/random`・`/dev/urandom` が空の通常ファイルになる

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の F-282（SB-11: namespace の mount の失敗の扱い）の作業中に `security::apply_dev_mount` を読んで見つけた。

## 事象

- `apply_dev_mount` は `/dev` に tmpfs を被せてから、`/dev/null` などを **同じパス** から bind mount していた。
- tmpfs を被せた後は元のデバイスノードがそのパスから見えないので、先に作った空のファイルを自分自身へ bind するだけになり、
  サンドボックスの中の `/dev/null` などはデバイスではなく tmpfs 上の空の通常ファイルになる。
- `/dev/null` への書き込みは tmpfs に溜まり（メモリが増える）、`/dev/urandom`・`/dev/zero` の読み取りはすぐ EOF になる。
  veil 本体は乱数を `getrandom(2)` で得るので乱数の質への影響は無いが、`/dev/*` を使うライブラリ（Wasm の WASI 等）は誤動作し得る。

## 影響

- P3（`enable_sandbox = true` かつ `sandbox_unshare_mount`・`sandbox_mount_dev` が有効（どちらも既定で有効）のとき）。

## 修正（v0.10.0）

- tmpfs を被せる前に元のノードを `O_PATH` で開き、`/proc/self/fd/N` から bind する。開けないノードは作らない（警告）。
- bind の失敗はマウントの失敗として扱う（F-282 の SB-11 と同じく `allow_security_failures = false` なら起動を止める）。
- 試験: `security::sandbox_mount_tests`（root のときだけ。子プロセスでマウント名前空間を作り、`/dev/null` などがデバイスのままであることを確かめる）。
