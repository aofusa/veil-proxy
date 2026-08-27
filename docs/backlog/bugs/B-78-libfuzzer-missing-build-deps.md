# B-78: container_security の libFuzzer フェーズがビルド依存不足で一度も実行されていなかった

## 事象

`tools/container_security/run.sh`（`SKIP_LIBFUZZER=0`）の **libfuzzer フェーズが必ず失敗**し、
`suite_summary.json` で `"name": "libfuzzer", "status": "failed"` になる。
**ファジングが 1 回も実行されていない**（クラッシュを検出していないのではなく、
そもそもターゲットのビルドが始まっていない）。

```
--- stderr
Missing dependency: cmake

thread 'main' panicked at aws-lc-sys-0.41.0/builder/main.rs:575:40:
called `Result::unwrap()` on an `Err` value:
  "Required build dependency is missing. Halting build."
Error: failed to build fuzz script: ... -Zsanitizer=address ... --bin hpack_decode
```

cmake を入れると次は bindgen で止まる。

```
thread 'main' panicked at bindgen-0.72.1/lib.rs:616:27:
Unable to find libclang: "couldn't find any valid shared libraries matching:
 ['libclang.so', ...], set the `LIBCLANG_PATH` environment variable ..."
```

## 原因

libFuzzer は `rustlang/rust:nightly-bookworm` コンテナ内で実行されるが、
このイメージには **cmake も libclang も入っていない**。

通常のビルドでは `aws-lc-sys` は**事前生成バインディング**を使うため両者とも不要だが、
`cargo fuzz` が付ける **sanitizer 用 `RUSTFLAGS`（`-Zsanitizer=address` 等）**が乗ると
`aws-lc-sys` は **cmake ビルダ経路**を選び、さらにその経路が bindgen（= libclang）を要求する。

コード側の不具合ではなく、**計測・検証コンテナのツール不足**である。

## 影響

- **libFuzzer によるファジング（HPACK / TOML / HTTP/2 フレーム / HTTP/1 ヘッダー境界 /
  リクエストスマグリング / io_uring executor / gRPC フレーム / QPACK / HTTP/3 フレームの
  計 9 ターゲット）が実質ゼロカバレッジだった。**
- 同じ理由で ASAN / TSAN パイプライン（`run_libfuzzer_asan.sh` / `run_libfuzzer_tsan.sh`、
  既定スキップ）も動かない。

## 改修

3 つのランナー（plain / ASAN / TSAN）に、コンテナ内で cmake・clang・libclang-dev を
必要時のみ導入し `LIBCLANG_PATH` を解決するガードを追加する。

### 実装時に踏んだ落とし穴（2 件）

いずれも `docker run ... bash -c "…"` の**ホスト側二重引用符の中**に追記したことによる。

1. **コメント内の `"` が文字列を途中で閉じた。** その結果 `docker run` は別物のコマンドを
   受け取り、**何も実行せずに終了コード 0 を返した**。ログには
   `libfuzzer start` と `libfuzzer 完了` の 2 行しか残らず、**成功したように見える**。
   → 引用符を含めない書き方にした。
2. **`$(...)` / `${...}` をエスケープせずに書いた。** ホスト側シェルで先に展開されてしまい、
   コンテナ内では評価されない。既存コードが `\${tdir}` / `\$(basename …)` と
   エスケープしているのはこのため。
   → `\$` でエスケープし、その理由をコメントで明示した。

**教訓**: このフェーズは「終了コード 0」だけでは成功と判定できない。
`libfuzzer target=<name>` の行数と `libfuzzer: ok` の有無まで確認すること
（`run.sh` の集計は `libfuzzer: ok` を見ているため failed を正しく検出できていた）。

## 検証

修正後、9 ターゲットすべてが実際にビルド・実行され、`Done … runs` を 9 回記録、
**クラッシュ・リーク検出ゼロ**、`libfuzzer: ok` を出力することを確認した。
