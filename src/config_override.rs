//! コマンドライン引数（`-o`/`--override`）による config.toml 値の上書き（F-143）。
//!
//! `veil -o "server.threads = 1" -o 'tls.cert_path = /etc/veil/cert.pem'` のように、
//! config.toml をロードする直前に任意のキーを上書きする。起動時のみならず
//! ホットリロード（SIGHUP）・`-t` 検証にも同じオーバーライドが適用される
//! （後述の [`apply_to_toml_str`] を通じて設定ロード経路が一本化されているため）。
//!
//! ## 構文
//!
//! 1 個の `--override` 引数は `<path> = <toml-value>` の形（`=` 前後の空白は任意）。
//!
//! - `<path>` はドット区切りのキーパス（例: `server.threads`、`tls.cert_path`、
//!   `http3.mmsg_batch_size`）。各セグメントは次のいずれか:
//!   - 裸のキー: `[A-Za-z0-9_-]+`
//!   - クオート文字列（`"..."` または `'...'`）: キーにドットを含む場合に使う
//!   - 10 進数の配列インデックス（親が配列である場合のみ有効。例: `l4.0.listen`）
//!   - 利便性のため、セグメントを TOML のセクション記法風に `[ ]` で囲んでよい
//!     （例: `[server].threads = 1` は `server.threads = 1` と等価）。解釈前に
//!     セグメント先頭の `[` と末尾の `]` を 1 個だけ取り除く。
//! - `<toml-value>` は `toml` クレートで TOML 値としてパースする（`1`、`"str"`、
//!   `true`、`1.5`、`[1, 2]`、`{ a = 1 }` などがそのまま使える）。裸のトークンが
//!   TOML 値としてパースできない場合は原則エラーだが、利便性のため **クオート・
//!   角括弧・波括弧のいずれも含まない場合に限り** 1 回だけ文字列リテラルとして
//!   再解釈する（`--override 'tls.cert_path = /etc/veil/cert.pem'` のようなパスを
//!   クオート無しで書けるようにするため）。これらの文字を含みながら TOML として
//!   不正な値は曖昧さを避けるためハードエラーにする。
//!
//! ## 適用規則
//!
//! パスを辿りながら存在しない中間テーブルは自動生成する。中間ノードが既に存在し
//! テーブル/配列以外の種類だった場合はパスを含む明確なエラーにする。配列インデックス
//! は親が配列であることと、インデックスが範囲内であることを要求する（範囲外は
//! エラー。配列の自動拡張はしない）。

use std::sync::OnceLock;

/// パスの 1 セグメント。
#[derive(Debug, Clone, PartialEq)]
enum Segment {
    /// テーブルのキー（裸のキーまたはクオート文字列由来）。
    Key(String),
    /// 配列のインデックス（10 進数の裸セグメント由来）。
    Index(usize),
}

/// 1 個の `--override` 引数をパースした結果。
#[derive(Debug, Clone)]
pub struct ConfigOverride {
    path: Vec<Segment>,
    value: toml::Value,
    /// エラーメッセージ用に元の引数文字列を保持する。
    raw: String,
}

/// 起動時に一度だけ確定するオーバーライド集合。
///
/// `--override` は CLI 引数解析直後、設定ファイルを読む前に確定するため
/// `OnceLock` で十分（実行中に再設定する必要はない）。
static GLOBAL_OVERRIDES: OnceLock<Vec<ConfigOverride>> = OnceLock::new();

/// グローバルなオーバーライド集合を確定させる。
///
/// `src/entry.rs::run()` の冒頭、`CliArgs::parse()` の直後・最初の設定ファイル読み込み
/// より前に一度だけ呼ぶ。2 回目以降の呼び出しは無視される（起動時に一度だけ呼ばれる
/// 想定のため、テスト以外では通常発生しない）。
pub fn set_global_overrides(overrides: Vec<ConfigOverride>) {
    let _ = GLOBAL_OVERRIDES.set(overrides);
}

/// 現在有効なオーバーライド集合を返す（未設定なら空スライス）。
pub fn global_overrides() -> &'static [ConfigOverride] {
    GLOBAL_OVERRIDES.get().map(|v| v.as_slice()).unwrap_or(&[])
}

/// `__veil_override = <value>` という 1 行ドキュメントとしてパースするための
/// 中間ラッパー。トップレベルの値としてでなくフィールド値として TOML 値パーサを
/// 呼び出すことで、整数・文字列・配列・インラインテーブルなど任意の値表現を
/// 単一の経路で受け付けられる。
#[derive(serde::Deserialize)]
struct ValueWrapper {
    __veil_override: toml::Value,
}

/// `<toml-value>` 側の文字列を `toml::Value` にパースする。
///
/// 通常は TOML 値としてパースする。失敗し、かつ入力にクオート・角括弧・波括弧の
/// いずれも含まれない場合に限り、文字列リテラルとして再解釈する（利便性のための
/// フォールバック。曖昧さを避けるため対象を「記号を含まない裸のトークン」に限定する）。
fn parse_value(raw: &str) -> Result<toml::Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("value is empty".to_string());
    }
    let doc = format!("__veil_override = {}", trimmed);
    match toml::from_str::<ValueWrapper>(&doc) {
        Ok(w) => Ok(w.__veil_override),
        Err(e) => {
            let has_symbol = trimmed
                .chars()
                .any(|c| matches!(c, '"' | '\'' | '[' | ']' | '{' | '}'));
            if has_symbol {
                Err(format!("invalid TOML value '{}': {}", trimmed, e))
            } else {
                // クオート無しの裸トークン（記号を含まない）は文字列リテラルとして扱う。
                Ok(toml::Value::String(trimmed.to_string()))
            }
        }
    }
}

/// `s` の中で最初に現れる「クオートの外にある」`=` の位置で `(path, value)` に分割する。
///
/// パス側のクオートされたセグメント内に `=` が含まれていても split 対象にしない
/// ようにするため、クオート状態を追跡しながら走査する。
fn split_top_level_eq(s: &str) -> Result<(&str, &str), String> {
    let mut in_quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match in_quote {
            Some(q) => {
                if c == q {
                    in_quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    in_quote = Some(c);
                } else if c == '=' {
                    return Ok((&s[..i], &s[i + 1..]));
                }
            }
        }
    }
    Err(format!(
        "missing '=' in override '{}': expected '<path> = <value>'",
        s
    ))
}

/// パス文字列をドット区切りの生セグメント（テキスト、クオート由来かどうか）に分解する。
fn tokenize_path(path: &str) -> Result<Vec<(String, bool)>, String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut in_quote: Option<char> = None;

    for c in path.chars() {
        match in_quote {
            Some(q) => {
                if c == q {
                    in_quote = None;
                } else {
                    current.push(c);
                }
            }
            None => match c {
                '"' | '\'' => {
                    in_quote = Some(c);
                    quoted = true;
                }
                '.' => {
                    segments.push((current.clone(), quoted));
                    current.clear();
                    quoted = false;
                }
                _ => current.push(c),
            },
        }
    }

    if in_quote.is_some() {
        return Err(format!("unterminated quote in path '{}'", path));
    }
    segments.push((current, quoted));
    Ok(segments)
}

/// セグメント先頭の `[` と末尾の `]` を 1 個だけ取り除く（TOML セクション記法風の
/// 利便性シンタックス）。
fn strip_brackets(s: &str) -> &str {
    let s = s.strip_prefix('[').unwrap_or(s);
    s.strip_suffix(']').unwrap_or(s)
}

/// 生セグメント（テキスト・クオート由来かどうか）を [`Segment`] に変換する。
fn to_segment(text: &str, was_quoted: bool) -> Result<Segment, String> {
    if was_quoted {
        if text.is_empty() {
            return Err("empty quoted path segment".to_string());
        }
        return Ok(Segment::Key(text.to_string()));
    }

    let stripped = strip_brackets(text);
    if stripped.is_empty() {
        return Err("empty path segment".to_string());
    }
    if stripped.chars().all(|c| c.is_ascii_digit()) {
        let idx: usize = stripped
            .parse()
            .map_err(|_| format!("invalid array index '{}'", stripped))?;
        return Ok(Segment::Index(idx));
    }
    if stripped
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Ok(Segment::Key(stripped.to_string()));
    }
    Err(format!(
        "invalid path segment '{}': must be [A-Za-z0-9_-]+, a quoted string, or a decimal index",
        text
    ))
}

impl ConfigOverride {
    /// 1 個の `--override` 引数文字列をパースする。
    pub fn parse(s: &str) -> Result<ConfigOverride, String> {
        let (path_str, value_str) = split_top_level_eq(s)?;
        let path_str = path_str.trim();
        if path_str.is_empty() {
            return Err(format!("empty path in override '{}'", s));
        }

        let raw_segments = tokenize_path(path_str)?;
        let mut path = Vec::with_capacity(raw_segments.len());
        for (text, was_quoted) in &raw_segments {
            path.push(to_segment(text, *was_quoted)?);
        }

        let value = parse_value(value_str)?;

        Ok(ConfigOverride {
            path,
            value,
            raw: s.to_string(),
        })
    }
}

/// パスを 1 段階たどり、次のノードへの可変参照を返す（最終セグメントより前段用）。
/// 存在しない中間テーブルはここで生成する。
fn navigate<'a>(
    node: &'a mut toml::Value,
    segment: &Segment,
    path_so_far: &str,
) -> Result<&'a mut toml::Value, String> {
    match segment {
        Segment::Key(key) => match node {
            toml::Value::Table(table) => Ok(table
                .entry(key.clone())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()))),
            other => Err(format!(
                "'{}' is not a table (found {}), cannot descend into key '{}'",
                path_so_far,
                other.type_str(),
                key
            )),
        },
        Segment::Index(idx) => match node {
            toml::Value::Array(arr) => {
                let len = arr.len();
                arr.get_mut(*idx).ok_or_else(|| {
                    format!(
                        "index {} out of bounds for array '{}' (len {})",
                        idx, path_so_far, len
                    )
                })
            }
            other => Err(format!(
                "'{}' is not an array (found {}), cannot index with {}",
                path_so_far,
                other.type_str(),
                idx
            )),
        },
    }
}

/// 1 個のオーバーライドを TOML 値木へ適用する。
fn apply_override(root: &mut toml::Value, ov: &ConfigOverride) -> Result<(), String> {
    let mut node = root;
    let mut path_so_far = String::new();

    for (i, seg) in ov.path.iter().enumerate() {
        if i > 0 {
            path_so_far.push('.');
        }
        match seg {
            Segment::Key(k) => path_so_far.push_str(k),
            Segment::Index(n) => path_so_far.push_str(&n.to_string()),
        }

        let is_last = i == ov.path.len() - 1;
        if !is_last {
            node = navigate(node, seg, &path_so_far)?;
            continue;
        }

        // 最終セグメント: 既存の値を上書き（テーブルなら insert、配列なら要素置換）。
        match seg {
            Segment::Key(key) => match node {
                toml::Value::Table(table) => {
                    table.insert(key.clone(), ov.value.clone());
                }
                other => {
                    return Err(format!(
                        "'{}' is not a table (found {}), cannot set key '{}'",
                        path_so_far,
                        other.type_str(),
                        key
                    ));
                }
            },
            Segment::Index(idx) => match node {
                toml::Value::Array(arr) => {
                    let len = arr.len();
                    if *idx >= len {
                        return Err(format!(
                            "index {} out of bounds for array '{}' (len {})",
                            idx, path_so_far, len
                        ));
                    }
                    arr[*idx] = ov.value.clone();
                }
                other => {
                    return Err(format!(
                        "'{}' is not an array (found {}), cannot set index {}",
                        path_so_far,
                        other.type_str(),
                        idx
                    ));
                }
            },
        }
    }

    Ok(())
}

/// TOML 文字列を [`global_overrides`] を適用した上で `T` にデシリアライズする。
///
/// `src/config.rs` の設定ロード経路（起動時・ホットリロード・`-t` 検証すべて）の
/// **単一チョークポイント**。オーバーライドが 0 件のときは `toml::Value` への
/// 中間変換を経ない高速経路（従来どおり `toml::from_str` 直呼び）を通る。
pub fn apply_to_toml_str<T>(s: &str) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    let overrides = global_overrides();
    if overrides.is_empty() {
        return toml::from_str::<T>(s).map_err(|e| format!("TOML parse error: {}", e));
    }

    let mut root: toml::Value =
        toml::from_str(s).map_err(|e| format!("TOML parse error: {}", e))?;

    for ov in overrides {
        apply_override(&mut root, ov)
            .map_err(|e| format!("config override error: {} (from '{}')", e, ov.raw))?;
    }

    root.try_into::<T>()
        .map_err(|e| format!("config override error: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn parse_bare_path_and_integer_value() {
        let ov = ConfigOverride::parse("server.threads = 1").unwrap();
        assert_eq!(
            ov.path,
            vec![
                Segment::Key("server".into()),
                Segment::Key("threads".into())
            ]
        );
        assert_eq!(ov.value, toml::Value::Integer(1));
    }

    #[test]
    fn parse_no_surrounding_spaces() {
        let ov = ConfigOverride::parse("server.threads=1").unwrap();
        assert_eq!(ov.value, toml::Value::Integer(1));
    }

    #[test]
    fn parse_bracket_form_first_segment() {
        let ov = ConfigOverride::parse("[server].threads = 1").unwrap();
        assert_eq!(
            ov.path,
            vec![
                Segment::Key("server".into()),
                Segment::Key("threads".into())
            ]
        );
    }

    #[test]
    fn parse_quoted_segment_with_dot() {
        let ov = ConfigOverride::parse(r#"tls."a.b" = 1"#).unwrap();
        assert_eq!(
            ov.path,
            vec![Segment::Key("tls".into()), Segment::Key("a.b".into())]
        );
    }

    #[test]
    fn parse_single_quoted_segment() {
        let ov = ConfigOverride::parse("tls.'x y' = 1").unwrap();
        assert_eq!(
            ov.path,
            vec![Segment::Key("tls".into()), Segment::Key("x y".into())]
        );
    }

    #[test]
    fn parse_array_index_segment() {
        let ov = ConfigOverride::parse("l4.0.listen = \"0.0.0.0:9000\"").unwrap();
        assert_eq!(
            ov.path,
            vec![
                Segment::Key("l4".into()),
                Segment::Index(0),
                Segment::Key("listen".into())
            ]
        );
        assert_eq!(ov.value, toml::Value::String("0.0.0.0:9000".to_string()));
    }

    #[test]
    fn parse_bare_string_without_quotes_falls_back() {
        // 記号を含まない裸トークンは TOML パース失敗時に文字列として再解釈される。
        let ov = ConfigOverride::parse("tls.cert_path = /etc/veil/cert.pem").unwrap();
        assert_eq!(
            ov.value,
            toml::Value::String("/etc/veil/cert.pem".to_string())
        );
    }

    #[test]
    fn parse_various_toml_value_kinds() {
        assert_eq!(
            ConfigOverride::parse("a = true").unwrap().value,
            toml::Value::Boolean(true)
        );
        assert_eq!(
            ConfigOverride::parse("a = 1.5").unwrap().value,
            toml::Value::Float(1.5)
        );
        assert_eq!(
            ConfigOverride::parse("a = [1, 2]").unwrap().value,
            toml::Value::Array(vec![toml::Value::Integer(1), toml::Value::Integer(2)])
        );
        let table_val = ConfigOverride::parse("a = { a = 1 }").unwrap().value;
        assert!(matches!(table_val, toml::Value::Table(_)));
    }

    #[test]
    fn parse_invalid_symbol_bearing_token_is_hard_error() {
        // 波括弧を含みつつ TOML として不正な値はフォールバックせずエラー。
        assert!(ConfigOverride::parse("a = {invalid}").is_err());
    }

    #[test]
    fn parse_missing_eq_is_error() {
        assert!(ConfigOverride::parse("server.threads").is_err());
    }

    #[test]
    fn parse_invalid_bare_key_char_is_error() {
        assert!(ConfigOverride::parse("server.thr@ads = 1").is_err());
    }

    #[test]
    fn apply_creates_nested_tables() {
        let mut root: toml::Value = toml::from_str("").unwrap();
        let ov = ConfigOverride::parse("server.threads = 4").unwrap();
        apply_override(&mut root, &ov).unwrap();
        assert_eq!(
            root.get("server").unwrap().get("threads").unwrap(),
            &toml::Value::Integer(4)
        );
    }

    #[test]
    fn apply_overwrites_existing_key() {
        let mut root: toml::Value = toml::from_str("[server]\nthreads = 1\n").unwrap();
        let ov = ConfigOverride::parse("server.threads = 8").unwrap();
        apply_override(&mut root, &ov).unwrap();
        assert_eq!(
            root.get("server").unwrap().get("threads").unwrap(),
            &toml::Value::Integer(8)
        );
    }

    #[test]
    fn apply_array_index_replaces_element() {
        let mut root: toml::Value = toml::from_str(
            "[[l4]]\nlisten = \"0.0.0.0:8000\"\n[[l4]]\nlisten = \"0.0.0.0:8001\"\n",
        )
        .unwrap();
        let ov = ConfigOverride::parse("l4.1.listen = \"0.0.0.0:9999\"").unwrap();
        apply_override(&mut root, &ov).unwrap();
        assert_eq!(
            root.get("l4")
                .unwrap()
                .get(1)
                .unwrap()
                .get("listen")
                .unwrap(),
            &toml::Value::String("0.0.0.0:9999".to_string())
        );
    }

    #[test]
    fn apply_array_index_out_of_bounds_is_error() {
        let mut root: toml::Value = toml::from_str("[[l4]]\nlisten = \"0.0.0.0:8000\"\n").unwrap();
        let ov = ConfigOverride::parse("l4.5.listen = \"0.0.0.0:9999\"").unwrap();
        assert!(apply_override(&mut root, &ov).is_err());
    }

    #[test]
    fn apply_type_mismatch_is_error() {
        // server がスカラーの場合、そこへ降りようとするとエラーになる。
        let mut root: toml::Value = toml::from_str("server = 1\n").unwrap();
        let ov = ConfigOverride::parse("server.threads = 1").unwrap();
        assert!(apply_override(&mut root, &ov).is_err());
    }

    #[test]
    fn apply_to_toml_str_fast_path_without_overrides_matches_direct_parse() {
        // オーバーライドが無関係なフィールドのみを追加する場合でも、既知フィールド
        // （x）の結果は toml::from_str の直接呼び出しと一致する（テスト実行順序に
        // よってはこのプロセスで既に別テストが set_global_overrides 済みのことがある
        // ため、「高速経路を通ること」自体ではなく最終結果の一致を確認する）。
        #[derive(Deserialize, PartialEq, Debug)]
        struct Cfg {
            x: i64,
        }
        let direct: Cfg = toml::from_str("x = 1").unwrap();
        let via_helper: Cfg = apply_to_toml_str("x = 1").unwrap();
        assert_eq!(direct, via_helper);
    }

    // `GLOBAL_OVERRIDES` はプロセス全体で 1 度しか set できない（OnceLock）。
    // グローバル状態を汚染する set_global_overrides の呼び出しは、この 1 テストに
    // 集約する（他のユニットテストは global_overrides() 経由の関数を呼ばない）。
    #[test]
    fn apply_to_toml_str_applies_global_override_to_config_struct() {
        #[derive(Deserialize)]
        struct ServerSection {
            threads: i64,
        }
        #[derive(Deserialize)]
        struct Cfg {
            server: ServerSection,
        }

        set_global_overrides(vec![ConfigOverride::parse("[server].threads = 9").unwrap()]);

        let cfg: Cfg = apply_to_toml_str("[server]\nthreads = 1\n").unwrap();
        assert_eq!(cfg.server.threads, 9);
    }
}
