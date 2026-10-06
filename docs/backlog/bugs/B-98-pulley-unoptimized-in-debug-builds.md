# B-98: デバッグビルドで Pulley インタープリタが最適化されず、WASM が 100 倍遅い（BSD の E2E フレーク）

## 事象

FreeBSD aarch64 の E2E で HTTP/3 + WASM 系のテストが並行実行時にタイムアウトした（B-97）。
WAF モジュールを通るリクエストを HTTP/1.1 で単独に投げても、**1 回 22〜23 秒**かかっていた
（初回だけでなく毎回）。

## 原因

NetBSD 全アーキ・FreeBSD/OpenBSD aarch64 は wasmtime を Pulley インタープリタで動かす（B-55）。
E2E はデバッグビルド（`opt-level = 0`）の veil を使うため、インタープリタの命令ディスパッチ
ループ（`pulley-interpreter` クレート）も最適化なしでコンパイルされ、ネイティブ実行比で
桁違いに遅くなっていた。

## 修正

ルート `Cargo.toml` に `[profile.dev.package.pulley-interpreter] opt-level = 3` を追加し、
dev プロファイルでもこのクレートだけ最適化する（小さいクレートなのでビルド時間への影響は小さい。
Linux 等の Cranelift ターゲットでは実行経路に乗らないので挙動は不変）。

FreeBSD aarch64 のデバッグビルドで、WAF を通るリクエストが **22 秒 → 0.2 秒**。

release ビルドは元々最適化されているので影響しない。HTTP/3 のメインループが WASM の完了を
待つ構造（B-97）は別件として残る。
