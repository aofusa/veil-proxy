# F-63: ログ出力先の分離ルーティング（app / error / access）と Landlock 自動許可

## 機能説明

`config.toml` の `[logging]` / `[access_log]` で、3 系統のログ出力先を **個別に** 指定できるようにする。

| 系統 | 対象レベル | 設定キー | 未指定時のデフォルト | `type` フィールド |
|------|-----------|----------|----------------------|-------------------|
| アプリ本体ログ | INFO / WARN / DEBUG / TRACE | `[logging].app_file_path` | **標準出力 (stdout)** | `"app"` |
| エラーログ | ERROR | `[logging].error_file_path` | **標準エラー出力 (stderr)** | `"error"` |
| アクセスログ | （アクセスログ） | `[access_log].file_path` | **標準出力 (stdout)** | `"access"` |

加えて:

- ログ出力先に **ファイルパスが指定された場合**、その **親ディレクトリ** を `[security].landlock_write_paths` に **自動追加** する（Landlock 有効時にログ書き込みが拒否されるのを防ぐ。ローテーション生成ファイルも同ディレクトリ配下のため親ディレクトリ許可が適切）。
- 各ログ行に `type: "app" | "error" | "access"` の識別フィールドを付与し、混在出力でも判別可能にする（text 形式は `type=...`、JSON 形式は `"type":"..."`）。

## 現状（改修前）

- `[logging]` は ftlog 単一 root（`file_path` 指定でファイル、未指定で **stderr**）に **INFO〜ERROR を混在** 出力。レベルによる出力先分離は不可（ftlog はターゲット接頭辞でのみ appender を振り分け、レベル振り分け機能を持たない）。
- `[access_log]`（`access-log` feature）は専用スレッド（`src/access_log.rs`）で分離出力。未指定時は **stderr**。feature 無効時は `log_access` が ftlog `info!` にフォールバック。
- `type` 識別フィールドなし。

## 改修内容

1. **レベル振り分け（app/error）**: ftlog の `FtLogFormat` が生成する msg 先頭に 1 バイトのルーティング用センチネル（app=`0x01` / error=`0x02`、レベルから決定）を埋め込み、root writer（`LogRoutingWriter`）がセンチネルを読んで app / error の出力先へ振り分け・センチネルを除去する。app/error ログは低頻度（リクエストごとではない）ため走査コストはホットパス規則に抵触しない。ファイル出力は従来どおり `ftlog::appender::FileAppender`（日次ローテーション）を内部 writer として使用。
2. **`type` フィールド**: 統合フォーマッタ `AppLogFormat`（text/json 両対応）が `type` を出力。`type` は `target == "access"` なら `access`、`ERROR` なら `error`、それ以外は `app`。
3. **access ログ**: `AccessLogConfig` の未指定時デフォルトを stderr→**stdout** に変更。`build_json_log` / `build_text_log` に `type=access` を追加。feature 無効時のフォールバックは `info!(target: "access", ...)`（レベル INFO のため app ストリーム＝既定 stdout に出力、`type=access` で識別可能）。
4. **Landlock 自動許可**: 起動時（`entry.rs`）に app/error/access の各ファイルパスの親ディレクトリを `landlock_write_paths` へ重複排除しつつ追加。

> 補足: 旧実装のレガシー `[logging].file_path`（app/error 兼用フォールバック）は不要のため削除済み。出力先は `app_file_path` / `error_file_path` / `[access_log].file_path` で個別指定する。

## 受け入れ条件

- `[logging].app_file_path` / `error_file_path` 未指定で app→stdout・error→stderr、access→stdout。
- ファイルパス指定時、当該親ディレクトリが `landlock_write_paths` に含まれる。
- app/error/access ログに `type` フィールドが付与される。
- `access-log` feature の有無で分離／ftlog フォールバックが切り替わる（既存挙動維持）。
- `--no-default-features` および関連 feature 組み合わせでビルド可能。
- 既存ユニット／統合テストが通り、追加テストで新挙動を実証。

## 依存・リスク

- ホットパス規則: app/error 振り分けはリクエストごとではない低頻度ログのみ。アクセスログのホットパスは従来どおり `access_log.rs` 専用スレッド（feature 有効時）。
- 同一ファイルを app/error 双方に指定した場合は 2 つの BufWriter が同一ファイルへ書くため出力が交錯し得る（設定ミスの範疇、ドキュメントで注意喚起）。
