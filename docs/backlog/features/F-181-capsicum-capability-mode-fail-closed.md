# F-181: capsicum の capability mode を要求して入れないとき起動を中止する（fail-closed）

**状態: 完了（feat/f178-capsicum-config-reload）**

## 現状の問題

`[security] capsicum_capability_mode = true` を指定しても、次の場合は warn / error ログを出すだけで
**rights 制限のみの弱いサンドボックスのまま起動を続ける**（fail-open）。

- 構成が capability mode の条件を満たさない（Proxy ルート・`[upstreams]`・L4・h2c・HTTP/3・リダイレクトリスナー）
- `cap_enter(2)` が失敗した
- 全ワーカーのリスナー bind 待ち（最大約 30 秒）がタイムアウトした

運用者は要求した防御が効いていないことに気づきにくい。Proxy ルートを 1 本足すだけでサンドボックスが黙って
一段弱まる。他のサンドボックス（seccomp・Landlock・pledge・unveil）は失敗時に既定で起動を中止する
（`allow_security_failures = false`）方針で、capsicum だけが食い違っている。

## 改修

- 上の 3 つのいずれでも、`allow_security_failures = false`（既定）なら理由を出して **起動を中止**（終了コード 1）。
- `allow_security_failures = true` のときだけ、従来どおり警告して rights 制限のみで続行する。
- 判定は純関数（要求の有無・阻害要因・`allow_security_failures` → 入る / 制限のみで続行 / 中止）にして単体テストする。

既存の「静的配信以外 + `capsicum_capability_mode = true`」の構成は起動しなくなる（互換性は求めない方針）。
ガイドと `examples/config.toml` に明記する。Proxy ルートを capability mode で使えるようにするのは F-182。

## 受け入れ条件

- 単体: 判定関数の全組み合わせ。
- FreeBSD security-e2e: capability mode + Proxy ルートの構成は終了コード 1 で起動せず理由をログに出す。
  `allow_security_failures = true` なら警告して起動する。

## 検証（2026-10-10）

- Linux: 単体 1000・統合 54。clippy（`full --all-targets`、`--no-default-features`）警告なし
- FreeBSD 14.3 x86_64（QEMU/KVM）: 単体 962/0・統合 54/0。security-e2e PASS
  （capability mode + Proxy ルートは終了コード 1 で中止し理由をログに出す。`allow_security_failures = true` なら
  警告して rights 制限のみで起動し 200 を返す）
