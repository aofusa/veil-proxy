# F-242: アプリケーションのログとメッセージを英語だけにし、チケット番号を出さない

**状態: 完了（feat/v083-release、2026-10-21。v0.8.3 で公開）**

## 目的

veil が実行中に出す文字列を、英語だけにし、チケット番号（`F-153`・`B-52` など）を含めないようにする。対象は次のとおり。

- ログ（ftlog の `error!`〜`trace!`）
- 標準出力・標準エラーへの出力（`println!`・`eprintln!`）
- 起動時の設定エラー
- panic・`expect` のメッセージ

ライブラリ（Rust・Python・C）として使った場合も、同じ文字列が利用者のログに出る。

## 今の状態（2026-10-21 の main = v0.8.2）

ほとんどのログは既に英語である。テストを除いた文字列のリテラルを字句単位で調べた結果が次のもの（docs 用の文字列を除く）。

### 日本語を含むもの（約 30 か所）

| ファイル | 内容 |
|---|---|
| `src/cache/resolve.rs` | 静的配信のパス解決（openat2 `RESOLVE_BENEATH`・capsicum・フォールバック）の `info!`・`warn!` が 6 件。どれも `F-153:` で始まる。ほかに `debug_assert` の 1 件 |
| `src/security.rs` | capsicum の dirfd の登録の `info!`・`warn!` が 6 件。`SYSTEMD_SYSCALL_FILTER` のテキスト（コメント行が日本語） |
| `src/entry.rs` | capsicum の dirfd の登録の失敗の `warn!` が 3 件 |
| `src/config/loaded.rs` | File ルートの canonicalize の失敗の `warn!` が 1 件 |
| `src/proxy/h2.rs`・`src/runtime/reactor/executor.rs` | 不変条件の `expect` が 3 件 |
| `src/pool.rs`・`src/cache/compressed.rs`・`src/http2/hpack/encoder.rs`・`src/mesh/xds/translate.rs` | `debug_assert!` などのメッセージが 6 件 |

### チケット番号を含むもの（日本語のものを除く）

| ファイル | 内容 |
|---|---|
| `src/wasm/types.rs` | `[wasm] interpreter=false is ignored on OpenBSD; ... (B-52)`。NetBSD の同じ文は `(F-140)` |
| `src/config/validate.rs` | 廃止した設定キーのエラー `"{} was removed (F-200): {}"` |
| `src/lib.rs` | FreeBSD で `jemalloc` を選んだときの `compile_error!`（`(B-89)`） |
| `build.rs` | 対応外の `target_os` の panic（`(F-120/F-125/F-140)`） |

- テストの中（`#[cfg(test)]`・`tests.rs`・`tests/`）の `assert!` などのメッセージには日本語が多い。これらは利用者には出ないので対象外にする。
- Python（`crates/veil-python/python/`）と C（`crates/veil-capi/`）のメッセージには、日本語もチケット番号も無かった。
  docstring は F-241 で英語にする。

## 設計

### 規則

- テスト以外の Rust の文字列のリテラルには、CJK の文字とチケット番号の形（`\b[FB]-[0-9]+\b`）を入れない。
  - 対象: `src/`・`crates/*/src/`・`build.rs`・`crates/*/build.rs`
  - doc 用の文字列（`#[doc = ..]` と、doc を組み立てるマクロの引数）もこの規則に含める（F-241 と一致させる）。
- ログの文はログの文としてだけで分かるように書く。「何が起きたか・何を使うか・何をすればよいか」を書き、
  チケット番号の代わりにキーや機能の名前を書く。

  | 今 | 直した後 |
  |---|---|
  | `F-153: 静的配信のパス解決はフォールバック canonicalize() を使用します（…）` | `static files: resolving paths with canonicalize() (no static root registered or not supported on this platform)` |
  | `[wasm] interpreter=false is ignored on OpenBSD; Pulley interpreter is always used (B-52)` | `[wasm] interpreter=false is ignored on OpenBSD; the Pulley interpreter is always used` |
  | `{} was removed (F-200): {}` | `{} was removed: {}`（hint の文の側で移行先のキーを示す） |

- チケット番号はログの呼び出しのすぐ上の `//` コメントへ移す（追跡のため）。
- ログのレベル・頻度・ログを出す場所は変えない（文だけを変える）。ホットパスのログを増やさない。

### 戻りを防ぐ検査

- `tools/lint/check-messages.py` を足す（Python 3、依存無し）。
  - Rust の字句を、文字列（通常・raw・byte）・文字・コメントの区別がつく程度に読む。
  - テストを除外する。除外するのは次のもの:
    - `#[cfg(test)]` の付いた項目（`mod` と `fn`）の波括弧の範囲
    - `#[test]` の関数
    - `tests.rs` と `tests/` の下のファイル
  - 残った文字列のリテラルに CJK またはチケット番号の形があれば、`ファイル:行` と文字列を出して失敗する。
  - 例外の指定は設けない。日本語が要る値（例: HPACK の符号化の試験の入力）はテストの中にしか無いので、除外で足りる。
    本番のコードで要るときは Unicode のエスケープで書く。
- CI（`ci.yml` の lint のジョブ）で回す。手元では `python3 tools/lint/check-messages.py` で回せる。
- 検査の側の誤りを確かめる単体テストを付ける（`tools/lint/test_check_messages.py`）。
  - 確かめる例: テストの mod の中の日本語は無視する。raw 文字列の `"` で字句がずれない。`//` の中は無視する。
  - 理由: AGENTS.md「container_security の判定は本当に走ったかを見る」。

### 文書

- `AGENTS.md` のコーディング規約に 1 行足す: 「ログ・エラー・panic の文字列と rustdoc は英語で、チケット番号を
  入れない（`tools/lint/check-messages.py`）。コメント（`//`）は日本語でよい」。
- `docs/guide/observability.md`・`security.md`（英日）にログの例の文を載せていれば、新しい文に合わせる。

## 受け入れ条件

- `python3 tools/lint/check-messages.py` が 0 件で通り、CI でも回る。
- `tools/lint/test_check_messages.py` が通る。
- 単体・統合・E2E（`./tests/e2e_setup.sh test`、`full,epoll` も）が通る。ログの文を見ている試験があれば、新しい文に合わせて直す。
- FreeBSD の VM で capsicum の起動ログが英語で出る（`bsd-vm.sh freebsd <arch> security-e2e`）。
  直す文の多くが capsicum の経路にあり、Linux のビルドではコンパイルされないため。

## 範囲外

- テストのメッセージ。
- `tools/`・`tests/` の台本の出力（開発者向け）。
- `//` のコメント。

## 実装の結果

- 40 件（日本語 約 30・番号 約 10）を英語に。残りは doc の文字列で F-241 で直した。
- `tools/lint/check-messages.py`（字句・テストの除外）と単体テスト 13 件。CI の `messages` のジョブ。
- 単体 1085 件が通った。capsicum の経路のログは BSD の VM の試験（v0.8.3 の全プラットフォームの確認）で見る。

## 関連

- [F-241](F-241-docsrs-english-only.md)（rustdoc の英語化。規則と検査の考え方を揃える）
- [F-245](F-245-version-0-8-3.md)
