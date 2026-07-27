# B-53: `tests/e2e_setup.sh` の `head -c` が OpenBSD で使えず large.txt が 0 バイトになる

## 事象

OpenBSD 7.9 amd64（QEMU VM）で `tests/e2e_setup.sh test` を実行すると、
**圧縮 / バッファリング / キャッシュ系の E2E が一斉に失敗**する。

```
---- test_buffering_full_mode stdout ----
Full mode test: response time 27.131258ms, size 209
thread '<unnamed>' panicked at tests/e2e_tests.rs:5572:5:
Large file should be > 1000 bytes

---- test_compression_gzip stdout ----
Large file should be compressed, got Content-Encoding: None

---- test_buffering_adaptive_threshold_switch stdout ----
Adaptive threshold switch test: small=239 bytes, large=209 bytes
Large response should be larger than small response
```

「大きいはずのファイル」が小さいレスポンスしか返さない、という形で共通している。

## 原因

フィクスチャ生成が **GNU 拡張の `head -c`** を使っていた。

```sh
head -c 10000 /dev/zero | tr '\0' 'A' > "${FIXTURES_DIR}/backend1/large.txt"
```

OpenBSD の `head(1)` に `-c` は無い:

```
$ head -c 10000 /dev/zero | tr "\0" "A" | wc -c
head: unknown option -- c
usage: head [-count | -n count] [file ...]
       0
```

パイプの左が即エラー終了しても **リダイレクト `>` はファイルを作る**ため、
`large.txt` が **0 バイト**で出来上がる。`set -e` でも
パイプライン全体の終了ステータスは末尾の `tr`（成功）になるため検出されない。

その結果、「1024 バイトの圧縮閾値を超える」「バッファリングのモード差が出る」
といった前提が崩れ、依存する E2E がまとめて落ちていた。

## 影響

- OpenBSD（および `head -c` を持たない他の非 GNU 環境）で E2E の
  compression / buffering / cache 系が **コードとは無関係に**失敗する。
- FreeBSD の `head` は `-c` を持つため影響しない（実測で当該テストは通過）。

## 修正

POSIX の `dd` を使う（3 箇所をループにまとめた）:

```sh
for _dir in backend1 backend2 backend_h2c; do
    dd if=/dev/zero bs=10000 count=1 2>/dev/null | tr '\0' 'A' \
        > "${FIXTURES_DIR}/${_dir}/large.txt"
done
```

## 関連

- B-51（OpenBSD E2E の SIGSEGV。本件はその修正後に見えるようになった別問題）
- B-52（OpenBSD で wasm 実行時に veil が SIGSEGV する。同じ E2E 実行で併発）
