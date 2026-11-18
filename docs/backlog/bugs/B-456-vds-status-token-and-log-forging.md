# B-456: 参照の VDS 制御プレーンが `/v1/status` をトークンなしで返し、要求のノード ID・NACK の文をそのままログに書き、Python 版は NaN の保留時間で回り続けた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「VDS control plane serves /v1/status without checking the bearer token, even when --token-file is set」・
「Client-supplied node id and NACK message written verbatim to the VDS server log (log forging)」・
「Python VDS server prints client-controlled node id and NACK message unescaped to stdout」・
「Unvalidated poll_timeout_secs accepts NaN, so the request thread busy-loops in wait_change until the snapshot
changes」（4 件）から確認。
後の検証の「/v1/status returns all node state and the snapshot parse error without the bearer-token check, even when a token is configured」も同じ指摘。

## 事象

- `tools/vds/server`（Rust）と `tools/vds/vds_server.py` の `GET /v1/status` はトークンを見ず、`--token-file` を
  付けても誰にでもノードの一覧・ACK 済みの版・最後の NACK の文・スナップショットの解析エラー（toml のエラーは該当の
  行を含む）を返した。同梱の Kubernetes のマニフェストはクラスタの中へ公開している。
- NACK のときに要求のノード ID・`error_detail.message` を `{}`・f-string でそのまま標準出力へ書き、改行で偽の行
  （「snapshot version …」等）を足したり、端末のエスケープシーケンスを埋めたりできた。
- Python 版は `poll_timeout_secs` を検査せず、`NaN`（`json.loads` は字句の `NaN` を受ける）を渡すと `wait_change` の
  `left <= 0` が成り立たず、`Condition.wait(nan)` はすぐ返るので、要求のスレッドがスナップショットが変わるまで回り続けた。
  型の違う `node`・`error_detail`（配列等）は例外で接続を落とした。

## 対応

- 両方の `/v1/status` に discovery と同じトークンの検査を足した（無い・違えば 401）。
- ログに書くノード ID・NACK の文は Rust が `{:?}`、Python が `repr` で書き（制御文字を書き換える）、512 文字で切る。
- Python 版: `NaN`・`Infinity` の字句を JSON の誤りとして 400、`poll_timeout_secs` は有限の数だけ受けて
  `[0, --hold]` に収める（数でなければ 400）。`node`・`error_detail`・`version_info` の型の違いと不正な
  `Content-Length` も 400。トークンの比較はバイト列で行う（ASCII 以外の値で例外にならない）。
- `tools/k8s/kind-e2e.sh` と自己完結型 E2E の状態の確認はトークンを付ける。利用者ガイド（`vds.md` の英日）に書いた。

## テスト

- 自己完結型 E2E の VDS の試験（`standalone_e2e`。状態はトークン付きで読み、トークン無しは 401）。
