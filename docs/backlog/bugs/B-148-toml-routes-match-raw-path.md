# B-148: TOML のルートが、正規化していないパスで照合・検査していた

**状態: 完了（feat/v080-release、2026-10-18。F-236 の再走査の F7）**

## 症状

HTTP/1.1・HTTP/2・HTTP/3 とも、生の要求先でルートを照合し、そのまま上流へ送っていた。`/admin` を `allowed_ips` で制限し、`/` が
同じ上流へ中継する設定で、`/%61dmin`・`/public/../admin` は `/admin` に一致せず `/` で通り、正規化する上流は管理画面を返した。
HTTP/1.1 の絶対形式の要求先（`http://host/admin`）も同じだった。B-138 はメッシュだけを直していた。

## 修正

- パスの正規化（`src/path_norm.rs`。メッシュから移して共有）を、各プロトコルの入口で 1 回だけ行う（HTTP/1.1 は要求先、HTTP/2 は
  ヘッダの検証の後の `:path`、HTTP/3 はフィールドセクションの検査の後の `:path`）。ルートの照合・検査・上流への要求が同じパスを使う。
- `[security] path_normalization`（`"base"` 既定・`"merge_slashes"`・`"off"`）。`%2F`・`%5C` はそのまま残す。
- HTTP/1.1 の絶対形式は origin 形式に直し、それ以外の origin 形式でない要求先（CONNECT と OPTIONS の `*` を除く）は 400。HTTP/2・
  HTTP/3 の `:path` は origin 形式か `*` だけを受ける。
- 正規化の要らないパスは 1 回の走査だけで確保しない。

## 試験

`path_norm::tests`、E2E。

## 関連

F-236、B-138。
