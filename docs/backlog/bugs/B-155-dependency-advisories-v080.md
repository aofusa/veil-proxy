# B-155: 依存の RUSTSEC 勧告（rustls・quinn-proto・pyo3・wasmtime）と container_security の集計の誤り

## 出典

v0.8.0 のリリース前の `tools/container_security/run.sh`（`SKIP_CARGO_AUDIT=0 SKIP_CARGO_DENY=0` を含むフル拡張）。

## 勧告

| クレート | 版 | 勧告 | 対応 |
|---|---|---|---|
| rustls | 0.23.45 | RUSTSEC-2026-0346（クライアントのセッション値の `Debug` が秘密を出す）・0347（ECH 拒否後に外側のサーバへ送信済みデータ）・0348（Brotli の証明書展開が認証前に最大 1GiB 確保）・0349（未認証の ServerHello で以後の接続を古典的な鍵交換へ落とせる） | 0.23.46 |
| quinn-proto | 0.11.17 | RUSTSEC-2026-0338・0340・0343（相手から panic させられる）。試験・計測用の HTTP/3 クライアントだけが使う | 0.11.19 |
| pyo3 | 0.23.5 | RUSTSEC-2025-0020（`PyString::from_object` のバッファ溢れ）・RUSTSEC-2026-0177（`PyCFunction::new_closure` の `Sync` 境界の欠落）。`crates/veil-python` だけが使う（どちらの API も使っていない） | 0.29.3 |
| wasmtime | 36.0.17 | RUSTSEC-2026-0363（component model のハンドル表の上限がインスタンスごと） | 36.0.18（LTS）。`third_party/wasmtime` の vendoring も作り直した |

`fxhash`（unmaintained、wasmtime の `profiling` 経由の推移的依存）は警告のみで残す（`deny.toml` の
`unmaintained = "workspace"`）。

pyo3 0.29 への更新は名前の変更だけ（`Python::with_gil` → `attach`、`allow_threads` → `detach`、
`downcast` → `cast`、`GILOnceCell` → `PyOnceLock`、`#[pyclass]` の `FromPyObject` は `from_py_object` で明示）。

ルートの `Cargo.lock` に加え、追跡している他の `Cargo.lock`（`examples/api/*`・`tests/*`・`tools/*`）も
同じ版に上げた。

## container_security の集計・設定の誤り

- `resource_exhaustion` は検出 0 件でも `failed` と集計されていた（`lib/report.sh` は「<フェーズ>: ok」で合格と
  判定するが、スクリプトは `done (findings=0)` しか出していなかった）。
- gitleaks が git 管理外の作業ディレクトリ（`tools/perf/results/`・`.claude/`）と、試験用の公開サンプル
  （Istio の JWT サンプル・kind の E2E の basic_auth の資格情報）を検出していた。許可リストに足した。

## ステータス

完了（v0.8.0）。
