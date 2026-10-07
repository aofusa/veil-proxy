# third_party/wasmtime — veil 向け vendoring（B-55）

## 由来

crates.io の [`wasmtime` 36.0.17](https://crates.io/crates/wasmtime/36.0.17)（LTS）をそのままコピー
（ローカルの `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/wasmtime-36.0.17/` から）。

> **2026-10: 40.0.4 → 36.0.17（LTS）へ移した（B-79）。** cargo-audit が wasmtime 40 系に
> 16 件の勧告（RUSTSEC-2026-0096 = aarch64 Cranelift の誤コンパイルによるサンドボックス脱出
> など critical を含む）を出した。40 系はすでに保守終了で、修正版は 36.0.x（LTS）と 43 以降に
> ある。43 以降（最新 49.0.2）は MSRV が Rust 1.96 で、packaging の docker イメージ
> （1.89 固定のものがある）や pkgsrc の rust-bin を含むツールチェーン全体の更新が要るため、
> MSRV 1.86 の LTS 36 系を選んだ。veil が使う API（core module・async + fuel yield・
> epoch 割り込み・プーリングアロケータ・Cranelift / Pulley）は 36 でもそのまま使える。
> 本体の crates.io 依存（Linux / macOS / Windows / FreeBSD x86_64 / OpenBSD x86_64）も
> 同じ 36.0.17 に揃えてある。
`Cargo.toml` / `Cargo.toml.orig` / `LICENSE` / `README.md` / `build.rs` / `src/` を取り込み、
`Cargo.lock` / `.cargo-ok` / `tests/` / `proptest-regressions/` は取り込んでいない
（veil のワークスペースビルドには不要で、`tests/` は crates.io 公開物とは無関係の
統合テスト専用コードのため）。

## 差分は 2 点だけ（+ tests/ 非取り込みに伴う後始末 1 点）

### 1. `Cargo.toml`

- `[package] name = "wasmtime"` → `name = "veil-wasmtime"`。
  **`[lib] name = "wasmtime"` は変更していない。**
- `[lints.rust]` に `dead_code = "allow"` を追加。
- `[[test]]`（crates.io 版の統合テスト宣言。`tests/` を取り込んでいないため）を削除。
- `[lints.rust]` の `unused-lifetimes` / `unused-macro-rules` をアンダースコア表記
  （`unused_lifetimes` / `unused_macro_rules`）へ変更。cargo 1.99 以降はハイフン表記を
  非推奨としてビルドのたびに manifest 警告を出すため（2026-10、Linux aarch64 VM の
  cargo 1.99.0 で検出。アンダースコア表記は旧 cargo でも同じ意味で解釈される）。

### 2. `build.rs`

`main()` 内の `has_native_signals` 算出に `veil_force_no_native_signals` を AND し、
以下の 3 ターゲットで強制的に `false` にする:

- `netbsd`（全アーキ）
- `openbsd` × `aarch64`
- `freebsd` × `aarch64`

これ以外のファイルは一切変更していない。

### 付随的な後始末: `Cargo.toml` の `[[test]]` 削除

`tests/` を取り込んでいないため（上記「由来」参照）、crates.io 版の `Cargo.toml` に
残っていた `[[test]]`（`custom_signal_handler` / `engine_across_forks` /
`host_segfault` / `pooling_alloc_near_oom` / `unload-engine`）はそのままだと
存在しないファイルを指す target 宣言になる。`cargo build`（lib のみ参照）は
これでも問題ないが、`cargo fmt` や `cargo clippy --all-targets` はターゲット列挙時に
ファイル存在チェックでハードエラーになるため、この 5 つの `[[test]]` ブロックを
削除している。実装（コンパイル対象になるソース）には一切影響しない、
純粋にマニフェスト上の後始末。

## なぜこの vendoring が必要か（B-55）

wasmtime 36 / 40 の `signals.rs` には（OpenBSD aarch64・NetBSD 向けの）上記 3 ターゲット向けの `ucontext` ベースのシグナルハンドラ
実装が存在せず、`has_native_signals = true` のままビルドすると
`compile_error!("unsupported platform")` になる。

`has_native_signals` は **wasmtime 自身の `build.rs`** が
`CARGO_CFG_TARGET_OS` / `CARGO_CFG_TARGET_ARCH` から導出する cfg であり、
依存側（veil）の cargo feature からは一切制御できない。したがって
`[patch.crates-io]` や feature 経由の回避策は使えず、`build.rs` 自体にパッチを当てる
以外に方法が無い。

対象ターゲットでは wasmtime を Pulley インタープリタ専用で実行する
（`src/wasm/registry.rs` で `Config::target("pulley64")` を強制。ネイティブコード生成を
行わないため、シグナルベースの trap（境界外アクセスを SIGSEGV で捕捉する方式）は
そもそも不要で、`has_native_signals = false` にすると自動的に有効になる
明示バウンドチェック方式で完全に代替できる。ランタイム設定側の追加変更は不要。

## なぜパッケージ名だけ変えて `[lib] name` は変えないのか

Linux / Windows / macOS / FreeBSD(x86_64) / OpenBSD(x86_64) は crates.io の
`wasmtime` 36.0.17 をそのまま使い、ソースも依存も一切変えない設計（B-55 の影響範囲限定）。
`[target.'cfg(...)'.dependencies]` でターゲット別に依存を出し分けているが、
もし依存キーを両方とも `wasmtime` のままにすると、cargo は

```
error: Dependency 'wasmtime' has different source paths depending on the build target
```

で解決を拒否する（同一キーに対し crates.io と path の 2 つの source が衝突するため）。
これを避けるため vendoring 側だけパッケージ名を `veil-wasmtime` に変え、
依存キーを `wasmtime`（crates.io 版）と `veil-wasmtime`（path 版）に分離した。

## `[workspace] exclude` が実際には効かないことについて

ルート `Cargo.toml` の `[workspace]` に
`exclude = ["third_party/wasmtime"]` を付けているが、cargo には
「`members` に含まれるディレクトリ配下の path はこの `exclude` が効かない」既知の
制約がある（[rust-lang/cargo#6745](https://github.com/rust-lang/cargo/issues/6745)）。
veil の `members` は `["."]`（ルート自身）なので、配下の `third_party/wasmtime` は
この exclude の対象にならず、`cargo metadata` の `workspace_members` には
実際には含まれてしまう（実測確認済み）。

それでも実害は無い。`cargo build` / `cargo clippy`（`--workspace` 無指定）は
デフォルトでカレントパッケージ（`veil`）のみを対象にし、依存クレートである
`veil-wasmtime` は通常の rustc コンパイルのみ（clippy lint は掛からない）。

ただし `cargo fmt` は**ワークスペース全体**が既定対象なので、暗黙メンバである
本 vendoring も整形対象に入ってしまう（第三者コードを rustfmt のバージョン差で
書き換えてしまうと「差分は build.rs と Cargo.toml だけ」という不変条件が壊れる）。
これを防ぐため本ディレクトリに **`rustfmt.toml`（`disable_all_formatting = true`）**
を置いている（差分 3 点目。ルート側の `cargo fmt --check` がゼロ差分で通ることを確認済み）。

このため vendoring 先を独立ワークスペース化する（`third_party/wasmtime/Cargo.toml` に
空の `[workspace]` を追加する）対処は**あえて行っていない**: 試したところ
cargo が `error: multiple workspace roots found in the same workspace` を返して
逆にビルド不能になった（root 側が member `"."` 配下として認識し続けるため、
子側の独立宣言と衝突する）。`exclude` はコードの意図を示す記述として残しつつ、
実際の安全性は上記のデフォルトスコープ挙動に依っている。

一方 cargo が `rustc --extern` に渡すのは **パッケージ名ではなく lib ターゲット名**
なので、`[lib] name = "wasmtime"` さえ据え置けば、どちらの経路でも Rust コード側からは
`use wasmtime::...;` のまま参照できる。そのため `src/` 以下は 1 行も変更していない。

## 新しい wasmtime バージョンへ追従する手順

1. `cargo update` 等で確認した新バージョンを、crates.io のレジストリキャッシュ
   （`~/.cargo/registry/src/index.crates.io-*/wasmtime-<new-version>/`）から取得する。
   無ければ `cargo fetch` 等で一度解決してキャッシュさせる。
2. `third_party/wasmtime/` の中身を新バージョンのコピーで丸ごと置き換える
   （`Cargo.toml` / `Cargo.toml.orig` / `LICENSE` / `README.md` / `build.rs` / `src/` のみ。
   `Cargo.lock` / `.cargo-ok` / `tests/` / `proptest-regressions/` は取り込まない）。
3. 上記「差分は 2 点だけ」を再適用する。
   - `Cargo.toml`: `name = "veil-wasmtime"` への変更 + `[lints.rust] dead_code = "allow"`
     （既存の `[lints.rust]` セクションがあればそこに追記、無ければ新設）+ ハイフン表記の
     lint 名（`unused-lifetimes` 等）のアンダースコア化。
   - `build.rs`: `has_native_signals` 算出への `veil_force_no_native_signals` の AND。
4. ルート `Cargo.toml` の `wasmtime` / `veil-wasmtime` 依存のバージョン指定
   （`version = "40.0.0"` 等）も新バージョンに合わせて更新する。
5. `git diff` で「差分は 2 点だけ」から逸脱していないか確認し
   （新バージョンで `build.rs` の周辺コードが変わっている場合は手動でマージ）、
   Linux ホストでの `cargo build --features full` / `cargo clippy` / `cargo fmt --check` が
   ゼロ警告で通ることを確認する。
