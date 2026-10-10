# B-111: FreeBSD で単一ファイルの File ルートがあると静的ルートの dirfd 登録が全滅する

**状態: 完了（feat/f178-capsicum-config-reload）**

## 事象

`security::capsicum::init_static_dirfds` は File ルートのパスを `O_DIRECTORY` で開く。単一ファイルを指す
File ルート（`mode = "memory"` 等）があると `ENOTDIR` で **関数全体が `Err` を返して中断** し、
他のディレクトリルートも登録されず `STATIC_ACTIVE` が立たない。

- capability mode では、ディレクトリルートの静的配信が絶対パス open（`ECAPMODE`）に落ちて全滅する。
- capability mode でなくても、F-153 の `O_RESOLVE_BENEATH` による封じ込め経路が無効になり、
  canonicalize の従来経路に戻る（性能面の退行）。

F-178（capability mode 下の設定リロード）の実装中に、コードを読んで見つけた。

## 修正

開けないルートは警告ログを出してそのルートだけ飛ばし、残りのルートの登録を続ける。単一ファイルの
File ルートは起動時に読み込む（`memory`）か、従来経路で扱う。capability mode 中のリロードでは、
登録済みディレクトリの配下にない単一ファイルのルートは F-178 の規則で拒否される。
