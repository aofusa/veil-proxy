# F-178: FreeBSD capsicum capability mode 下で SIGHUP の設定リロードを可能にする

**状態: 進行中（feat/f178-capsicum-config-reload）**

## 現状

`[security] capsicum_capability_mode = true` で `cap_enter(2)` した後の SIGHUP（および admin API のリロード）は
`Failed to reload configuration: Not permitted in capability mode` で失敗し、前の設定を保つ。証明書だけは
F-136 の dirfd 経由で `[tls] auto_reload` によりリロードできる。設定変更の反映には再起動が必要
（`docs/guide/running.md` に記載）。

capability mode に入れるのは静的配信専用の構成だけ（上流・L4・h2c・HTTP/3・リダイレクトリスナーがあると
`entry.rs` の適格判定で入らない）。

## 阻害要因（`config::reload_config` 経路、main `847ad50` 時点）

| # | 箇所 | 現象 |
|---|------|------|
| B1 | `load_config_without_tls` の `fs::read_to_string` | 絶対パス open → `ECAPMODE`（現在表面化している失敗） |
| B2 | `validate_config` の証明書 `exists()` | 絶対パス stat 失敗 → false →「TLS certificate file not found」 |
| B3 | `canonical_base_memoized`（スレッドローカルのメモ。リロードスレッドでは空） | `canonicalize` → `ECAPMODE` → 生パスへフォールバック。未登録パスの `is_dir` は `fs::metadata` で失敗 |
| B4 | `capsicum::STATIC_DIRS` は `OnceLock` | 新しい静的ルートは登録も open もできない |
| B5 | `access_log::reload_access_log_writer` | 新スレッドで出力ファイルを絶対パス open → 失敗すると **黙って stdout へフォールバック**（B1 を直すと踏む潜在不具合） |
| B6 | リロードで追加されたプロキシルート・`[upstreams]` | `connect` が `ECAPMODE`。リロードは成功して 502 を返し続ける |

## 改修案（採用: dirfd 方式、F-136 と同じ設計）

1. 設定ファイルの親ディレクトリ fd を `cap_enter` 前に登録し（`CAP_LOOKUP|CAP_READ|CAP_FSTAT|CAP_FCNTL`）、
   リロード時は `openat(O_RESOLVE_BENEATH)` で読む。名前は毎回引き直すので rename 置換にも追従する。
2. capability mode 中のリロードでは証明書の存在チェックを省く（TLS はリロード対象外で `current` を引き継ぐ）。
3. 静的ルートの canonical 解決は、起動時の登録表（設定文字列 → canonical パス → dirfd）から引く。
   未登録のルートを含む設定はリロードを拒否して前の設定を保つ。
4. capability mode の適格判定を関数化して起動時とリロード時で共用し、上流接続を要する構成
   （`Proxy` / `ProxyUpstream` ルート、`[upstreams]`、h2c、HTTP/3、リダイレクトリスナー）へのリロードを拒否する。
5. アクセスログは capability mode 中にスレッドを作り直さず、既存のライタースレッドへ開き直しを指示する。
   ログの親ディレクトリ fd を `cap_enter` 前に登録し、同じパスは `openat(O_CREAT|O_APPEND)` で開き直す
   （logrotate の move + SIGHUP が動く）。出力先パスの変更はリロード拒否。開き直しに失敗しても stdout へは
   切り替えない。

不採用: 特権分離ヘルパー（サンドボックス外の常駐プロセスで攻撃面が増える）、libcasper `cap_fileargs`
（範囲は同じで FFI 依存が増える）、設定 fd の開きっぱなし（rename 置換に追従しない）。

調査の詳細: `docs/artifacts/capsicum_sighup_config_reload_proposal_2026-10-10.md`（git 管理外）。

## 受け入れ条件

- FreeBSD VM の security-e2e で、capability mode 下の SIGHUP により応答ヘッダ等の変更が反映される。
- 未登録の静的ルート・プロキシルートを追加した設定の SIGHUP は拒否され、前の設定で応答し続ける。
- アクセスログを mv して SIGHUP すると、新しいファイルに追記される。
- 他 OS（Linux 等）のリロード挙動は変わらない。ホットパスは変えない。
- `docs/guide/running.md`（英日）と `examples/config.toml` を更新する。
