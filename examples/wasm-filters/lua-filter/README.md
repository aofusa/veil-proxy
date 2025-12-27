# Lua Filter

Proxy-WASM用の純粋Rust製Luaインタープリタフィルタ

## 概要

外部Luaライブラリを一切使用せず、ゼロから実装したLuaインタープリタ。
WASMサンドボックス環境で安全に動作します。

## ビルド

```bash
cargo build --target wasm32-wasip1 --release
```

出力: `target/wasm32-wasip1/release/lua_filter.wasm` (約524 KB)

## 設定例

```json
{
  "script": "function on_request() veil.set_request_header('X-Lua', 'true') return 'continue' end",
  "debug": true
}
```

---

## Lua API (veil.*)

| 関数 | 説明 |
|------|------|
| `veil.log(level, msg)` | ログ出力 (debug/info/warn/error) |
| `veil.get_request_header(name)` | リクエストヘッダ取得 |
| `veil.set_request_header(name, val)` | リクエストヘッダ設定 |
| `veil.get_response_header(name)` | レスポンスヘッダ取得 |
| `veil.set_response_header(name, val)` | レスポンスヘッダ設定 |
| `veil.get_path()` | リクエストパス取得 |
| `veil.get_method()` | HTTPメソッド取得 |
| `veil.send_local_response(status, body)` | レスポンス送信 |
| `veil.get_headers()` | 全ヘッダをテーブルで取得 |

---

## Lua互換性

### 対象バージョン

本実装は **Lua 5.4** の構文・機能をベースにしていますが、純粋Rust実装のため完全互換ではありません。

### バージョン別機能サポート

| 機能 | Lua 5.1 | Lua 5.2 | Lua 5.3 | Lua 5.4 | 本実装 |
|------|---------|---------|---------|---------|--------|
| 基本構文 | ✅ | ✅ | ✅ | ✅ | ✅ |
| クロージャ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 可変長引数 (...) | ✅ | ✅ | ✅ | ✅ | ✅ |
| 汎用for | ✅ | ✅ | ✅ | ✅ | ✅ |
| repeat-until | ✅ | ✅ | ✅ | ✅ | ✅ |
| goto/ラベル | ❌ | ✅ | ✅ | ✅ | ✅ |
| ビット演算子 | ❌ | ❌ | ✅ | ✅ | ✅ |
| 整数除算 (//) | ❌ | ❌ | ✅ | ✅ | ✅ |
| メタテーブル | ✅ | ✅ | ✅ | ✅ | ✅ |
| コルーチン | ✅ | ✅ | ✅ | ✅ | ❌ |
| 末尾呼び出し最適化 | ✅ | ✅ | ✅ | ✅ | ✅ |

---

## 機能詳細

### ✅ 完全サポート

#### データ型
| 型 | サポート | 備考 |
|----|---------|------|
| `nil` | ✅ | |
| `boolean` | ✅ | |
| `number` | ✅ | 64bit浮動小数点 |
| `string` | ✅ | UTF-8 |
| `table` | ✅ | HashMap実装 |
| `function` | ✅ | クロージャ対応 |
| `userdata` | ❌ | 非サポート |
| `thread` | ❌ | 非サポート |

#### 演算子
| カテゴリ | 演算子 | サポート |
|---------|--------|---------|
| 算術 | `+ - * / // % ^` | ✅ |
| 比較 | `== ~= < > <= >=` | ✅ |
| 論理 | `and or not` | ✅ (短絡評価) |
| 文字列 | `..` | ✅ |
| 長さ | `#` | ✅ |
| ビット | `& \| ~ << >>` | ✅ |

#### 制御構文
| 構文 | サポート |
|------|---------|
| `if/elseif/else/end` | ✅ |
| `while/do/end` | ✅ |
| `repeat/until` | ✅ |
| `for i=1,10 do/end` | ✅ |
| `for k,v in pairs(t) do/end` | ✅ |
| `break` | ✅ |
| `return` | ✅ |
| `do/end` | ✅ |
| `goto/::label::` | ✅ |

#### 関数
| 機能 | サポート |
|------|---------|
| 関数定義 | ✅ |
| ローカル関数 | ✅ |
| 無名関数 | ✅ |
| クロージャ | ✅ (upvalue capture) |
| 可変長引数 | ✅ |
| 複数戻り値 | ✅ |
| メソッド呼び出し (`:`) | ✅ |

---

### ✅ パターンマッチング

| パターン | 説明 | サポート |
|---------|------|---------|
| `.` | 任意の1文字 | ✅ |
| `%a` | アルファベット | ✅ |
| `%d` | 数字 | ✅ |
| `%s` | 空白文字 | ✅ |
| `%w` | 英数字 | ✅ |
| `%p` | 句読点 | ✅ |
| `%c` | 制御文字 | ✅ |
| `%l` | 小文字 | ✅ |
| `%u` | 大文字 | ✅ |
| `%x` | 16進数字 | ✅ |
| `%A %D ...` | 上記の補集合 | ✅ |
| `[set]` | 文字セット | ✅ |
| `[^set]` | 補集合セット | ✅ |
| `*` | 0回以上(貪欲) | ✅ |
| `+` | 1回以上(貪欲) | ✅ |
| `-` | 0回以上(非貪欲) | ✅ |
| `?` | 0回または1回 | ✅ |
| `^` | 先頭アンカー | ✅ |
| `$` | 末尾アンカー | ✅ |
| `()` | キャプチャ | ✅ |
| `%bxy` | バランスマッチ | ✅ |
| `%1-%9` | 後方参照 | ✅ |
| `%f[set]` | フロンティア | ✅ |

---

### ✅ 標準ライブラリ

#### 基本関数
| 関数 | サポート | 備考 |
|------|---------|------|
| `print` | ✅ | veil.log経由 |
| `tostring` | ✅ | |
| `tonumber` | ✅ | |
| `type` | ✅ | |
| `assert` | ✅ | |
| `error` | ✅ | |
| `pcall` | ✅ | エラーハンドリング対応 |
| `pairs` | ✅ | |
| `ipairs` | ✅ | |
| `next` | ✅ | |
| `select` | ✅ | |
| `setmetatable` | ✅ | メタメソッド完全対応 |
| `getmetatable` | ✅ | メタテーブル取得対応 |
| `rawget/rawset` | ✅ | メタテーブルをバイパスする完全実装 |
| `load` | ✅ | 文字列からのコード読み込み対応 |
| `loadfile` | ❌ | ファイルI/Oのため未実装 |
| `require` | ✅ | モジュール読み込み対応 |
| `dofile` | ❌ | ファイルI/Oのため未実装 |

#### string.*
| 関数 | サポート | 備考 |
|------|---------|------|
| `string.len` | ✅ | |
| `string.sub` | ✅ | |
| `string.upper` | ✅ | |
| `string.lower` | ✅ | |
| `string.find` | ✅ | パターン対応 |
| `string.match` | ✅ | パターン対応 |
| `string.gsub` | ✅ | パターン対応 |
| `string.gmatch` | ✅ | イテレータ実装 |
| `string.format` | ✅ | 全フォーマット対応 |
| `string.rep` | ✅ | |
| `string.reverse` | ✅ | |
| `string.byte` | ✅ | |
| `string.char` | ✅ | |
| `string.dump` | ⚠️ | 簡易実装（関数名のみ返却） |
| `string.pack/unpack` | ✅ | エンディアン指定対応 |

#### math.*
| 関数 | サポート |
|------|---------|
| `math.abs` | ✅ |
| `math.ceil` | ✅ |
| `math.floor` | ✅ |
| `math.max` | ✅ |
| `math.min` | ✅ |
| `math.sin/cos/tan` | ✅ |
| `math.asin/acos/atan` | ✅ |
| `math.sqrt` | ✅ |
| `math.log` | ✅ |
| `math.exp` | ✅ |
| `math.pow` | ✅ |
| `math.deg/rad` | ✅ |
| `math.random` | ✅ |
| `math.randomseed` | ✅ | シード値の設定が正しく動作 |
| `math.modf/fmod` | ✅ |
| `math.pi` | ✅ |
| `math.huge` | ✅ |
| `math.ult/tointeger` | ✅ |

#### table.*
| 関数 | サポート | 備考 |
|------|---------|------|
| `table.concat` | ✅ | 範囲指定対応 |
| `table.insert` | ✅ | 位置指定対応 |
| `table.remove` | ✅ | 戻り値対応 |
| `table.sort` | ✅ | 配列ソート |
| `table.pack` | ✅ | |
| `table.unpack` | ✅ | 開始位置指定 |
| `table.move` | ✅ | |

---

### ❌ 非サポート（WASM安全性のため除外）

| モジュール | 理由 |
|-----------|------|
| `os.*` | システムコール |
| `io.*` | ファイルI/O |
| `debug.*` | デバッグ機能 |
| `package.*` | 動的モジュール読み込み |
| `coroutine.*` | ✅ | コルーチン機能（create, resume, yield, status, wrap） |

#### utf8.*
| 関数 | サポート |
|------|---------|
| `utf8.len` | ✅ |
| `utf8.char` | ✅ |
| `utf8.codepoint` | ✅ |
| `utf8.offset` | ✅ |
| `utf8.charpattern` | ✅ |
| `utf8.codes` | ✅ | イテレータ実装 |

---

## サンプルスクリプト

```lua
function on_request()
    local path = veil.get_path()
    local method = veil.get_method()
    veil.log('info', method .. ' ' .. path)
    
    -- パターンマッチングでパス解析
    local id = string.match(path, '/users/(%d+)')
    if id then
        veil.set_request_header('X-User-ID', id)
    end
    
    -- 管理画面へのアクセス制御
    if string.find(path, '^/admin') then
        local auth = veil.get_request_header('Authorization')
        if not auth then
            veil.send_local_response(403, 'Forbidden')
            return 'stop'
        end
    end
    
    return 'continue'
end

function on_response()
    veil.set_response_header('X-Powered-By', 'veil-proxy-lua')
    return 'continue'
end
```

---

## コルーチン機能

### 概要

Luaのコルーチン機能を実装しています。コルーチンは、関数の実行を一時停止し、後で再開できる協調的マルチタスク機能です。

### サポート機能

| 関数 | 説明 |
|------|------|
| `coroutine.create(f)` | 関数`f`からコルーチンを作成します。戻り値は`thread`型です。 |
| `coroutine.resume(co, ...)` | コルーチン`co`を再開します。引数を渡すことができます。戻り値は`(ok, ...)`の形式で、`ok`は成功フラグ、その後にコルーチンから返された値が続きます。 |
| `coroutine.yield(...)` | コルーチンを一時停止し、呼び出し元に値を返します。コルーチン外で呼び出すとエラーになります。 |
| `coroutine.status(co)` | コルーチン`co`の状態を返します。`"suspended"`、`"running"`、`"dead"`のいずれかです。 |
| `coroutine.wrap(f)` | 関数`f`からコルーチンを作成し、それをラップした関数を返します。この関数を呼び出すと、コルーチンが再開されます。 |

### 使用例

```lua
-- 基本的な使用例
function counter()
    local i = 0
    while i < 3 do
        i = i + 1
        coroutine.yield(i)
    end
    return i
end

local co = coroutine.create(counter)
local ok1, val1 = coroutine.resume(co)  -- ok1 = true, val1 = 1
local ok2, val2 = coroutine.resume(co)  -- ok2 = true, val2 = 2
local ok3, val3 = coroutine.resume(co)  -- ok3 = true, val3 = 3
local ok4, val4 = coroutine.resume(co)  -- ok4 = true, val4 = 3 (return value)

-- coroutine.wrapの使用例
local wrapped = coroutine.wrap(counter)
local val1 = wrapped()  -- 1
local val2 = wrapped()  -- 2
local val3 = wrapped()  -- 3
local val4 = wrapped()  -- 3 (return value)
```

### 実装の制限

- **ネストしたコルーチン呼び出し**: サポートされていますが、複雑なネストは推奨されません。
- **ループ内でのyield**: サポートされていますが、一部の複雑なループ構造では動作しない可能性があります。
- **パフォーマンス**: 状態の保存・復元にオーバーヘッドがあります。
- **メモリ使用量**: コルーチンごとにスコープスタックのコピーが必要なため、メモリ使用量が増加します。

---

## 実装詳細

| コンポーネント | ファイル | 行数 |
|---------------|---------|------|
| 字句解析 | `src/lua/lexer.rs` | ~390 |
| 構文解析 | `src/lua/parser.rs` | ~860 |
| AST | `src/lua/ast.rs` | ~220 |
| 値型 | `src/lua/value.rs` | ~260 |
| パターン | `src/lua/pattern.rs` | ~770 |
| インタープリタ | `src/lua/interpreter.rs` | ~3,460 |
| **合計** | | **~6,280** |
