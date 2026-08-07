# F-143: コマンドライン引数による config.toml 値の上書き（`-o`/`--override`）

## 機能説明

`veil` は起動時に `-c`/`--config` で指定した `config.toml` を丸ごと読み込む方式のみを
提供しており、一部のキーだけをコマンドラインから差し替える手段が無かった。デプロイ環境
（コンテナのエントリポイント引数、CI のパラメータ化ビルド、動作検証時の一時的な値の
差し替え等）では、設定ファイル本体を都度書き換えたり環境ごとに複製したりせず、
`nginx -g`/Envoy の `--config-yaml` オーバーライド相当の仕組みで個別キーだけを
差し替えたいニーズがある。

## 改修内容

- 新規モジュール `src/config_override.rs`（`src/lib.rs` に `pub mod config_override;` を追加）。
  - `-o`/`--override` の引数文字列（`<path> = <toml-value>`、`=` 前後の空白は任意）を
    パースする `ConfigOverride::parse(&str) -> Result<ConfigOverride, String>`。
    - `<path>` はドット区切りのキーパス。セグメントは裸のキー（`[A-Za-z0-9_-]+`）・
      クオート文字列（`"..."`/`'...'`、キーにドットを含む場合用）・10 進数の配列
      インデックス（親が配列の場合。例: `l4.0.listen`）のいずれか。利便性のため
      セクション記法風の `[server].threads = 1` も `server.threads = 1` と等価に扱う
      （セグメント先頭 `[` と末尾 `]` を 1 個だけ剥がす）。
    - `<toml-value>` は `toml` クレートで TOML 値としてパースする。裸のトークンが
      TOML 値としてパース不能で、かつクオート・角括弧・波括弧のいずれも含まない場合に
      限り文字列リテラルとして 1 回だけ再解釈する（`-o "tls.cert_path = /etc/veil/cert.pem"`
      のような無クオートのパス指定を許容するため）。それ以外の不正値はハードエラー。
  - グローバルなオーバーライド集合を確定させる `set_global_overrides(Vec<ConfigOverride>)` /
    参照する `global_overrides() -> &'static [ConfigOverride]`（`OnceLock`、起動時に一度だけ
    確定）。
  - `apply_to_toml_str::<T: DeserializeOwned>(&str) -> Result<T, String>`: TOML 文字列を
    `global_overrides()` 適用込みで `T` にデシリアライズする。オーバーライドが 0 件の場合は
    `toml::Value` への中間変換を経ない高速経路（従来どおり `toml::from_str` 直呼び）を通る。
  - 適用時はパスをたどりながら存在しない中間テーブルを自動生成し、中間ノードが既に別種類
    （スカラー等）だった場合や配列インデックスが範囲外の場合はパスを含む明確なエラーにする
    （配列の自動拡張はしない）。
- `src/config.rs` の設定ロード経路を `apply_to_toml_str` 経由に統一（単一チョークポイント）:
  `load_config`（起動時）、`load_config_without_tls`（ホットリロード）、
  `load_logging_config`（ログ初期化前）、`test_config_file`（`-t`）、
  `collect_unveil_paths`（OpenBSD unveil 用の再パース）、
  `collect_macos_sandbox_paths`（macOS sandbox_init 用の再パース）。これにより
  **起動時・ホットリロード（SIGHUP）・`-t` 検証のすべてで同じオーバーライドが適用される**。
- `CliArgs`（`src/config.rs`）に `-o`/`--override`（`Vec<String>`、繰り返し指定可）を追加。
- `src/entry.rs::run()`: `CliArgs::parse()` の直後・最初の設定ファイル読み込みより前に
  各 `--override` 文字列をパースして `set_global_overrides` に渡す。パースエラー時は
  `eprintln!("veil: invalid --override: {}", e)` を出力して `exit(1)`。

## 受け入れ条件

- `veil -o "server.threads = 4"` で config.toml の `[server] threads` を上書きして起動できる。
- `veil -o "[server].threads = 4"`（ブラケット記法）が同じ結果になる。
- `veil -o "tls.cert_path = /etc/veil/cert.pem"`（無クオートの文字列値）が動く。
- 存在しない中間テーブル（例: 空の `[server]` セクションすら無いファイルへの
  `server.threads` 上書き）が自動生成される。
- 型不一致（例: スカラーへドット降下）・配列範囲外インデックスは明確なエラーで起動が
  失敗する（`-t` では非 0 終了・通常起動では `eprintln!` の後 `exit(1)`）。
- `cargo test --lib --features full` に `src/config_override.rs` のユニットテストが含まれ、
  各構文形式（裸/クオート/配列インデックス/ブラケット）・ネスト自動生成・型不一致エラー・
  オーバーライド 0 件時の高速経路・`Config` への実適用が通る。
- README.md / docs/readme/README.ja.md の CLI オプション表・使用例に記載済み。
- `examples/config.toml` の冒頭に上書き機能への言及がある。

## 依存・リスク

- 追加の外部依存は無し（既存の `toml`/`serde`/`clap` のみを使用）。
- ホットパス（データプレーン）には触れない（設定ロードは起動・リロード・検証時のみの
  コールドパス）。
- オーバーライドはプロセス起動時に一度だけ確定する設計（`OnceLock`）のため、実行中に
  `--override` の内容を動的に変更する用途は対象外（そのようなユースケースは Admin API /
  動的設定配信（F-04）の領域）。
