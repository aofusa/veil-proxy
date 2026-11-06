# B-311: E2E（`tests/e2e_setup.sh test`）が Windows で走らない（UDS の上流で起動せず、試験のビルドが実行中の veil.exe を置き換えられず、SIGHUP の試験がプロキシを終了させる）

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 の全 OS の試験で、Windows（ssh aofish、`full-windows`）の E2E を初めて全部走らせた。

1. プロキシが起動しない: E2E の `proxy.toml` が UDS の上流（`http(s)://unix:...`）を常に含み、Windows は設定のロードで拒否する。
2. 試験のビルドが失敗する: `cargo test --test e2e_tests` が（dev-dependencies の feature の統合で）veil を作り直し、
   起動中の `veil.exe` を置き換えられずに `アクセスが拒否されました (os error 5)`。
3. 13 件が落ちる: SIGHUP のリロードの試験が Git Bash の `kill -HUP` を送り、Windows ではプロキシが終了する（終了コード 129。
   Windows に SIGHUP は無い）。その後の試験が接続拒否で落ちた。

## 修正

- `tests/e2e_setup.sh`: Windows では UDS の上流・ルートを設定に入れない。サーバの起動前に E2E の試験をビルドしておく（Windows だけ）。
- UDS の試験（`upstream::test_e2e_uds_backend_*`）と SIGHUP の試験（`send_sighup` を使う 6 件）を `#[cfg(unix)]` にした。
- 合わせて B-310（UDP の `WSAECONNRESET`）を直した。

## 関連

- B-310
